//! `[prompt] file` — the operator's hand-pruned system prompt (#2938).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{expand_tilde, Config};

/// Session twin of `[prompt] file`: set by `--system-prompt-file` and
/// `/settings system-prompt`, read before the config.
pub const SYSTEM_PROMPT_FILE_ENV: &str = "NEWT_SYSTEM_PROMPT_FILE";

/// `[prompt]` — when `file` is set, its bytes REPLACE newt's assembled system
/// prompt entirely: no soul, no project context, no workspace header.
///
/// ```toml
/// [prompt]
/// file = "~/.newt/system-prompt.md"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Path of the replacement system prompt. `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// A replacement system prompt read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptOverride {
    pub path: PathBuf,
    /// The file's contents, byte for byte.
    pub text: String,
}

impl Config {
    /// The override file in force: the session env twin, else `[prompt] file`.
    pub fn system_prompt_file(&self) -> Option<PathBuf> {
        self.system_prompt_file_with(std::env::var(SYSTEM_PROMPT_FILE_ENV).ok())
    }

    pub(crate) fn system_prompt_file_with(&self, env: Option<String>) -> Option<PathBuf> {
        let configured = self.prompt.as_ref().and_then(|p| p.file.clone());
        env.filter(|s| !s.trim().is_empty())
            .or(configured.filter(|s| !s.trim().is_empty()))
            .map(|s| expand_tilde(&s))
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

    #[test]
    fn the_session_twin_beats_the_config_file() {
        let c = cfg("[prompt]\nfile = \"/etc/p.md\"\n");
        assert_eq!(c.system_prompt_file_with(None), Some("/etc/p.md".into()));
        assert_eq!(
            c.system_prompt_file_with(Some("/tmp/session.md".into())),
            Some("/tmp/session.md".into())
        );
        // A blank twin is "unset", not "override with nothing".
        assert_eq!(
            c.system_prompt_file_with(Some("  ".into())),
            Some("/etc/p.md".into())
        );
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
