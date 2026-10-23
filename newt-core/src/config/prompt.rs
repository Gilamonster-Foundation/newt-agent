//! `[prompt] file` — the operator's hand-pruned system prompt (#2938).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{expand_tilde, Config};

/// Session twin of `[prompt] file`, set by `/settings system-prompt` and read
/// before the config: a path overrides it, a blank value (`reset`) suppresses
/// it, and absence defers to it.
pub const SYSTEM_PROMPT_FILE_ENV: &str = "NEWT_SYSTEM_PROMPT_FILE";

/// `[prompt]` — when `file` is set, its bytes REPLACE newt's assembled system
/// prompt entirely: no soul, no project context, no workspace header.
///
/// ```toml
/// [prompt]
/// file = "~/.newt/system-prompt.md"
/// ```
///
/// Without `file`, `preset` and `components` shape the assembled prompt (#2933):
///
/// ```toml
/// [prompt]
/// preset = "default"
/// components = { off = ["three-cs"], on = [] }
/// ```
///
/// They are deltas on the preset, so a component added later is never silently
/// off. Dropping one changes what the model is told, never what it may do. They
/// shape the default soul only: a custom `soul.md` or the coach soul replaces it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Path of the replacement system prompt. `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Named set of components to start from; `default` is today's prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Component ids to switch on or off relative to the preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<ComponentDeltas>,
}

/// `[prompt] components = { on = [...], off = [...] }`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComponentDeltas {
    pub on: Vec<String>,
    pub off: Vec<String>,
}

/// A replacement system prompt read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptOverride {
    pub path: PathBuf,
    /// The file's contents, byte for byte.
    pub text: String,
}

impl Config {
    /// The components in force: `[prompt] preset` with its deltas applied.
    ///
    /// # Errors
    /// An unknown preset or component id, naming the valid ones.
    pub fn prompt_components(&self) -> Result<crate::prompt_components::ComponentSet, String> {
        let Some(p) = &self.prompt else {
            return Ok(Default::default());
        };
        let d = p.components.clone().unwrap_or_default();
        let set =
            crate::prompt_components::ComponentSet::resolve(p.preset.as_deref(), &d.on, &d.off)?;
        // `card` off is valid only while the tools that read it are hidden.
        let exposure = crate::ExposureSettings::from(self.tool_exposure());
        set.check_card(&|tool| exposure.operator_visible(tool))?;
        Ok(set)
    }

    /// The override file in force: the session env twin, else `[prompt] file`.
    pub fn system_prompt_file(&self) -> Option<PathBuf> {
        self.system_prompt_file_with(std::env::var(SYSTEM_PROMPT_FILE_ENV).ok())
    }

    /// The twin has three states: absent (use `[prompt] file`), a path, and
    /// present-but-blank — a session `reset`, which suppresses the configured
    /// file and restores the assembled prompt without touching the config.
    pub(crate) fn system_prompt_file_with(&self, env: Option<String>) -> Option<PathBuf> {
        let chosen = match env {
            Some(twin) => twin,
            None => self.prompt.as_ref().and_then(|p| p.file.clone())?,
        };
        (!chosen.trim().is_empty()).then(|| expand_tilde(&chosen))
    }

    /// The replacement system prompt, if an override file is configured.
    /// `Ok(None)` only when none is; a configured file that cannot be read,
    /// or is empty, is an `Err` — never a silent fallback to the assembled
    /// prompt, which would send something other than what the operator saved.
    pub fn system_prompt_override(&self) -> Result<Option<SystemPromptOverride>, String> {
        self.system_prompt_file()
            .map(SystemPromptOverride::read)
            .transpose()
    }
}

impl SystemPromptOverride {
    /// [`read`](Self::read) for a path as the operator typed it (`~/` expands).
    pub fn read_typed(path: &str) -> Result<Self, String> {
        Self::read(expand_tilde(path))
    }

    pub fn read(path: PathBuf) -> Result<Self, String> {
        Self::read_with(path, |p| std::fs::read_to_string(p))
    }

    /// [`read`](Self::read) with the file read injected.
    pub(crate) fn read_with(
        path: PathBuf,
        read: impl FnOnce(&std::path::Path) -> std::io::Result<String>,
    ) -> Result<Self, String> {
        let text =
            read(&path).map_err(|e| format!("system prompt file {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            return Err(format!("system prompt file {} is empty", path.display()));
        }
        Ok(Self { path, text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn absent_section_means_no_override() {
        let c = cfg("");
        assert!(c.prompt.is_none());
        assert_eq!(c.system_prompt_file_with(None), None);
    }

    /// #2933 PR5: `card` off needs the card's tools hidden, checked at load.
    #[test]
    fn card_off_is_refused_at_load_unless_its_tools_are_hidden() {
        let visible = cfg("[prompt]\ncomponents = { off = [\"card\"] }\n");
        let err = visible.prompt_components().unwrap_err();
        assert!(err.contains("/settings tools"), "{err}");
        assert!(visible.prepare_runtime().is_err());
        let hidden = cfg(
            "[tool_exposure]\nallow_list = [\"read_file\"]\n[prompt]\ncomponents = { off = [\"card\"] }\n",
        );
        assert!(!hidden.prompt_components().unwrap().is_default());
    }

    #[test]
    fn prompt_components_read_the_preset_and_deltas() {
        assert!(cfg("").prompt_components().unwrap().is_default());
        let c = cfg("[prompt]\npreset = \"default\"\ncomponents = { off = [\"three-cs\"] }\n");
        assert!(!c.prompt_components().unwrap().is_default());
        let bad = cfg("[prompt]\ncomponents = { off = [\"nope\"] }\n");
        assert!(bad.prompt_components().unwrap_err().contains("`nope`"));
    }

    /// An unknown component id is refused when the config is prepared for a run,
    /// naming the valid ids, not silently ignored.
    #[test]
    fn an_unknown_component_is_refused_at_load() {
        let c = cfg("[prompt]\ncomponents = { off = [\"nope\"] }\n");
        let err = c.prepare_runtime().expect_err("unknown id refused");
        let msg = err.to_string();
        assert!(msg.contains("`nope`") && msg.contains("three-cs"), "{msg}");
    }

    #[test]
    fn with_prompt_components_keeps_the_rest_and_drops_the_key_when_empty() {
        let text = "# mine\n[prompt]\nfile = \"/p.md\" # keep\n";
        let out = Config::with_prompt_components(text, "default", &[], &["three-cs"]).unwrap();
        assert!(
            out.contains("# mine") && out.contains("file = \"/p.md\" # keep"),
            "{out}"
        );
        assert!(cfg(&out).prompt_components().unwrap().summary() == "default, 1 off");
        let back = Config::with_prompt_components(&out, "default", &[], &[]).unwrap();
        assert!(
            !back.contains("components") && back.contains("# keep"),
            "{back}"
        );
        let fresh = Config::with_prompt_components("", "default", &[], &["three-cs"]).unwrap();
        assert!(cfg(&fresh).prompt_components().unwrap().summary() == "default, 1 off");
    }

    /// #2991 review: comments on the EDITED key survive, for a changed value and a
    /// same-value write alike.
    #[test]
    fn with_prompt_components_keeps_the_edited_keys_comments() {
        let text = "[prompt]\n# key comment\ncomponents = { off = [\"three-cs\"] } # keep trailing rationale\n";
        let same = Config::with_prompt_components(text, "default", &[], &["three-cs"]).unwrap();
        assert!(
            same.contains("# key comment") && same.contains("# keep trailing rationale"),
            "{same}"
        );
        let changed = Config::with_prompt_components(
            text,
            "default",
            &[],
            &["three-cs", "commit-attribution"],
        )
        .unwrap();
        assert!(
            changed.contains("# key comment")
                && changed.contains("# keep trailing rationale")
                && changed.contains("commit-attribution"),
            "{changed}"
        );
        assert!(cfg(&changed)
            .prompt_components()
            .unwrap()
            .summary()
            .contains("2 off"));
    }

    /// #2933 PR6: `preset` is written and read back, keeps its decoration, and is
    /// dropped again for `default`.
    #[test]
    fn with_prompt_components_writes_the_preset_and_keeps_its_comments() {
        let text = "[prompt]\n# which prompt\npreset = \"pi\" # minimal\n";
        let same = Config::with_prompt_components(text, "pi", &[], &[]).unwrap();
        assert!(
            same.contains("# which prompt") && same.contains("# minimal"),
            "{same}"
        );
        assert!(
            cfg(&same).prompt_components().is_err(),
            "pi needs the card tools hidden"
        );
        let tweaked = Config::with_prompt_components(
            "[tool_exposure]\nallow_list = [\"read_file\"]\n",
            "pi",
            &["readme"],
            &[],
        )
        .unwrap();
        let set = cfg(&tweaked).prompt_components().unwrap();
        assert_eq!(set.summary(), "pi, 1 changed");
        let back = Config::with_prompt_components(&tweaked, "default", &[], &[]).unwrap();
        assert!(
            !back.contains("preset") && !back.contains("components"),
            "{back}"
        );
    }

    #[test]
    fn the_session_twin_beats_the_config_file() {
        let c = cfg("[prompt]\nfile = \"/etc/p.md\"\n");
        assert_eq!(c.system_prompt_file_with(None), Some("/etc/p.md".into()));
        assert_eq!(
            c.system_prompt_file_with(Some("/tmp/session.md".into())),
            Some("/tmp/session.md".into())
        );
        // A blank twin is a session reset: it suppresses the configured file.
        assert_eq!(c.system_prompt_file_with(Some("  ".into())), None);
    }

    #[test]
    fn contents_are_kept_byte_for_byte() {
        let text = "  leading space\n\ttab, trailing newline lost? no.\n\n";
        let o = SystemPromptOverride::read_with("/p".into(), |_| Ok(text.to_string())).unwrap();
        assert_eq!(o.text, text);
    }

    #[test]
    fn an_unreadable_or_blank_file_is_an_error_never_a_fallback() {
        let missing = SystemPromptOverride::read_with("/p".into(), |_| {
            Err(std::io::ErrorKind::NotFound.into())
        });
        assert!(missing.unwrap_err().contains("/p"));
        let blank = SystemPromptOverride::read_with("/p".into(), |_| Ok(" \n".into()));
        assert!(blank.unwrap_err().contains("empty"));
    }
}
