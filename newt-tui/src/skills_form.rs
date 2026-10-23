//! `/settings skills [list] | on <name> | off <name>` (#2933).
//!
//! The typed door onto `[skills] off`: list the installed skills with what each costs
//! the context, or switch one off so the model is not offered it and cannot load it.
//! The write is the same comment-preserving read-modify-write every other persisted
//! setting takes; the chat loop re-resolves its config afterwards and rebuilds the
//! system prompt.
//!
//! An off skill stays installed. Turning it off changes what the model is offered, and
//! `use_skill`, a posture and a persona binding refuse it by name.

use newt_core::markup::table::{render_table, Align, Column};

/// The skill commands the `skills` setting understands, as `(keyword, argument)`.
pub(crate) fn parse(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    let (word, arg) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
    match word {
        "list" | "on" | "off" => Some((word, arg.trim())),
        _ => None,
    }
}

/// The config in force. One that does not resolve is an ERROR, never a default: a
/// fabricated default would let a write confirm itself against a config nobody has.
#[cfg(not(test))]
pub(crate) fn current() -> Result<newt_core::Config, String> {
    crate::migration_notices::read(|report| newt_core::Config::resolve_unpublished(report))
        .map(|c| newt_core::Config::clone(&c))
        .map_err(|e| format!("config does not resolve: {e:#}"))
}

/// The unit tier reads the mocked user file, like the other persisted settings.
#[cfg(test)]
pub(crate) fn current() -> Result<newt_core::Config, String> {
    let path = newt_core::Config::user_config_path().unwrap_or_default();
    let text = crate::settings_form::config_fs::read(&path);
    toml::from_str::<newt_core::Config>(&text).map_err(|e| format!("config does not resolve: {e}"))
}

/// `all on`, or `off: a, b`; `unknown (...)` while the config does not resolve.
pub(crate) fn status() -> String {
    match current() {
        Ok(cfg) => {
            let off = cfg.skills.map(|s| s.off).unwrap_or_default();
            if off.is_empty() {
                "all on".to_string()
            } else {
                format!("off: {}", off.join(", "))
            }
        }
        Err(e) => format!("unknown ({e})"),
    }
}

/// The listing: skill, state, what it costs every request (its index line), what it
/// costs when loaded (its body), and what it is for.
pub(crate) fn listing() -> Result<String, String> {
    let cfg = current()?;
    let skills = newt_skills::discover_paths(&cfg.skill_search_dirs());
    let (mut index_on, mut index_all) = (0, 0);
    let rows: Vec<Vec<String>> = skills
        .iter()
        .map(|s| {
            let off = cfg.skill_is_off(&s.name);
            let index = s.index_line().chars().count();
            index_all += index;
            if !off {
                index_on += index;
            }
            vec![
                s.name.clone(),
                if off { "off" } else { "on" }.to_string(),
                format!("{index} chars"),
                format!("{} chars", s.body.chars().count()),
                s.description.clone(),
            ]
        })
        .collect();
    if rows.is_empty() {
        return Ok("no skills installed".to_string());
    }
    let mut out = format!(
        "skills: {} of {} on; the index costs {index_on} chars in every request (all on: {index_all})\n",
        skills.iter().filter(|s| !cfg.skill_is_off(&s.name)).count(),
        skills.len()
    );
    out.push_str(&render_table(
        &[
            Column::new("skill"),
            Column::new("state"),
            Column::new("index").align(Align::Right),
            Column::new("body").align(Align::Right),
            Column::new("what it is for"),
        ],
        &rows,
    ));
    out.push_str(
        "\n/settings skills on|off <name> · an off skill is not offered and use_skill refuses it",
    );
    Ok(out)
}

/// Persist `off` and confirm `name` is in the state asked for (a project layer can
/// override the operator's file).
fn persist(off: &[String], name: &str, want_off: bool) -> Result<(), String> {
    let refs: Vec<&str> = off.iter().map(String::as_str).collect();
    crate::settings_form::persist_config(|text| newt_core::Config::with_skills_off(text, &refs))?;
    if current()?.skill_is_off(name) == want_off {
        Ok(())
    } else {
        Err(format!(
            "skills: saved to your config, but another config layer overrides `[skills] off`, so \
             `{name}` is NOT {} — change it there",
            if want_off { "off" } else { "on" }
        ))
    }
}

/// Run one skill command; the reply is what the operator reads.
pub(crate) fn apply(word: &str, arg: &str) -> Result<String, String> {
    match word {
        "list" => listing(),
        "on" | "off" => {
            // Refuse before writing when the config does not resolve.
            let cfg = current()?;
            if arg.is_empty() {
                return Err(format!(
                    "/settings skills {word} <name>; /settings skills lists them"
                ));
            }
            let installed = newt_skills::discover_paths(&cfg.skill_search_dirs());
            let Some(skill) = installed.iter().find(|s| s.name == arg) else {
                let names: Vec<&str> = installed.iter().map(|s| s.name.as_str()).collect();
                return Err(format!(
                    "unknown skill `{arg}`; installed: {}",
                    names.join(", ")
                ));
            };
            let want_off = word == "off";
            let mut off = cfg.skills.map(|s| s.off).unwrap_or_default();
            off.retain(|n| n != arg);
            if want_off {
                off.push(arg.to_string());
            }
            persist(&off, arg, want_off)?;
            let index = skill.index_line().chars().count();
            Ok(if want_off {
                format!(
                    "skill {arg}: off — not in the skill index (-{index} chars per request) and \
                     use_skill refuses it; the next request sends the rebuilt prompt"
                )
            } else {
                format!(
                    "skill {arg}: on — back in the skill index (+{index} chars per request); the \
                     next request sends the rebuilt prompt"
                )
            })
        }
        _ => unreachable!("parse only yields skill words"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_form::{apply_and_record, config_fs, Field};

    /// Two installed skills and a config that searches them, with decoration to keep.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let skill = dir.path().join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: the {name} skill\n---\nBody of {name}.\n"),
            )
            .unwrap();
        }
        let path = newt_core::Config::user_config_path().expect("a config path");
        config_fs::write(
            &path,
            &format!(
                "# mine\n[skills]\nsearch = [{:?}] # where\n",
                dir.path().display().to_string()
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn row(listing: &str) -> String {
        listing
            .lines()
            .find(|r| r.starts_with("| alpha"))
            .unwrap()
            .to_string()
    }

    #[test]
    fn only_the_skill_words_are_commands() {
        assert_eq!(parse("off alpha"), Some(("off", "alpha")));
        assert_eq!(parse(" list "), Some(("list", "")));
        for other in ["", "offal x", "alpha"] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    /// Off writes `[skills] off` and keeps the operator's comments; the listing shows the
    /// state and the saving; on removes the key again.
    #[test]
    fn off_and_on_persist_keep_decoration_and_are_journalled() {
        let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let (_dir, path) = fixture();
        let listing = apply("list", "").unwrap();
        assert!(
            listing.contains("alpha") && listing.contains("2 of 2 on"),
            "{listing}"
        );

        let said = apply_and_record(Field::Skills, "off alpha", "/settings skills").unwrap();
        assert!(
            said.contains("not in the skill index") && said.contains("use_skill refuses"),
            "{said}"
        );
        let saved = config_fs::read(&path);
        assert!(saved.contains("off = [\"alpha\"]"), "{saved}");
        assert!(
            saved.contains("# mine") && saved.contains("# where"),
            "{saved}"
        );
        assert_eq!(status(), "off: alpha");
        let listing = apply("list", "").unwrap();
        assert!(row(&listing).contains("| off "), "{listing}");

        apply("on", "alpha").unwrap();
        assert!(
            !config_fs::read(&path).contains("off"),
            "{}",
            config_fs::read(&path)
        );
        assert_eq!(status(), "all on");
    }

    /// An unknown skill, and a config that does not resolve, write nothing.
    #[test]
    fn unknown_skills_and_unresolvable_config_refuse_without_writing() {
        let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let (_dir, path) = fixture();
        let before = config_fs::read(&path);
        let err = apply("off", "nope").unwrap_err();
        assert!(
            err.contains("`nope`") && err.contains("alpha, beta"),
            "{err}"
        );
        assert!(apply("off", "").unwrap_err().contains("<name>"));
        assert_eq!(config_fs::read(&path), before);

        config_fs::write(&path, "[skills\nbroken").unwrap();
        let broken = config_fs::read(&path);
        let err = apply("off", "alpha").unwrap_err();
        assert!(err.contains("config does not resolve"), "{err}");
        assert_eq!(
            config_fs::read(&path),
            broken,
            "a config that does not resolve is not rewritten"
        );
    }

    /// The index the model sees drops an off skill (the builder's filter).
    #[test]
    fn the_prompt_index_drops_an_off_skill() {
        let (dir, _path) = fixture();
        let dirs = [dir.path().to_path_buf()];
        let all = crate::skills_index_for_prompt(&dirs, &|_| false).unwrap();
        assert!(all.contains("alpha") && all.contains("beta"), "{all}");
        let some = crate::skills_index_for_prompt(&dirs, &|n| n == "alpha").unwrap();
        assert!(!some.contains("alpha") && some.contains("beta"), "{some}");
    }
}
