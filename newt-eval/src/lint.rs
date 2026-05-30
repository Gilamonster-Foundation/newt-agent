//! Lint that catches `case.toml` prompts which re-specify the emission shape.
//!
//! The system prompt (newt-coder's `WHOLE_FILE_SYSTEM_PROMPT`) owns the
//! emission contract — it tells the model *how* to respond. A case prompt
//! that also says "respond with a unified diff only" contradicts it, and
//! different models resolve the system-vs-user conflict differently, biasing
//! the bake-off rankings (the bug fixed in #31). This lint makes that class
//! of mistake impossible to reintroduce (#33): case prompts describe the
//! *task*; they must not re-specify the format.
//!
//! It inspects only the `prompt` field (not `mock_response`, which
//! legitimately contains diffs), and runs as a `#[test]` so it gates CI via
//! `cargo test --workspace`.

use std::path::{Path, PathBuf};

use regex::Regex;

/// A single lint violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintError {
    /// The offending `case.toml`.
    pub case: PathBuf,
    /// Human-readable explanation.
    pub message: String,
}

impl std::fmt::Display for LintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.case.display(), self.message)
    }
}

/// The forbidden pattern: an emission-shape phrase followed (anywhere) by
/// "only". Case-insensitive and dotall, so a line-wrapped
/// "Respond\nwith a unified diff only." is still caught.
fn emission_shape_regex() -> Regex {
    Regex::new(
        r"(?is)(unified diff|whole file|complete( updated)? file|json object|fenced code block|patch only)\b.*\bonly\b",
    )
    .expect("static lint regex is valid")
}

/// Pull the `prompt` value out of a `case.toml` body.
fn extract_prompt(toml_text: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(toml_text).ok()?;
    value.get("prompt")?.as_str().map(str::to_string)
}

/// Lint a single prompt string. Returns the matched snippet if it
/// re-specifies the emission shape.
pub fn lint_prompt(prompt: &str) -> Option<String> {
    emission_shape_regex()
        .find(prompt)
        .map(|m| m.as_str().trim().to_string())
}

/// Walk `dir` recursively and lint every `case.toml`'s prompt. Returns the
/// list of violations (empty `Ok(())` when clean).
pub fn lint_case_prompts(dir: impl AsRef<Path>) -> Result<(), Vec<LintError>> {
    let mut errors = Vec::new();
    let mut stack = vec![dir.as_ref().to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("case.toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(prompt) = extract_prompt(&text) {
                if let Some(snippet) = lint_prompt(&prompt) {
                    errors.push(LintError {
                        case: path.clone(),
                        message: format!(
                            "prompt re-specifies the emission shape (\"{snippet}\") — \
                             the system prompt owns that; describe the task only"
                        ),
                    });
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        // Stable order for deterministic test output.
        errors.sort_by(|a, b| a.case.cmp(&b.case));
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn flags_unified_diff_only() {
        assert!(lint_prompt("Rename foo. Respond with a unified diff only.").is_some());
    }

    #[test]
    fn flags_line_wrapped_directive() {
        // The 002-style wrap: the phrase spans a newline.
        assert!(lint_prompt("Add a doc comment. Respond\nwith a unified diff only.").is_some());
    }

    #[test]
    fn flags_whole_file_only() {
        assert!(lint_prompt("Emit the whole file only.").is_some());
    }

    #[test]
    fn allows_a_clean_task_prompt() {
        assert!(lint_prompt(
            "Rename the function `greet` to `hello` and update the call site so tests pass."
        )
        .is_none());
    }

    #[test]
    fn allows_prompt_mentioning_files_without_directive() {
        // "file" appears but not as a "… file only" emission directive.
        assert!(lint_prompt("Move the helper into a new module file src/util.rs.").is_none());
    }

    #[test]
    fn lint_case_prompts_flags_a_dirty_case() {
        let tmp = tempdir().unwrap();
        let case = tmp.path().join("099-bad");
        std::fs::create_dir_all(&case).unwrap();
        std::fs::write(
            case.join("case.toml"),
            "name = \"099-bad\"\nprompt = \"\"\"\nDo a thing. Respond with a unified diff only.\n\"\"\"\n",
        )
        .unwrap();
        let err = lint_case_prompts(tmp.path()).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("emission shape"));
    }

    #[test]
    fn lint_case_prompts_passes_clean_tree() {
        let tmp = tempdir().unwrap();
        let case = tmp.path().join("001-ok");
        std::fs::create_dir_all(&case).unwrap();
        std::fs::write(
            case.join("case.toml"),
            "name = \"001-ok\"\nprompt = \"\"\"\nRename greet to hello.\n\"\"\"\n",
        )
        .unwrap();
        assert!(lint_case_prompts(tmp.path()).is_ok());
    }
}
