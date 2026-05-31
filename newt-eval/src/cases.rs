//! TestCase definition + TOML loader.
//!
//! Each case lives at `newt-eval/cases/NNN-name/` and consists of:
//! - `case.toml` — metadata + mock response (see [`TestCase`]).
//! - `workspace/` — the initial filesystem state. Copied verbatim into a
//!   tempdir at the start of each run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One self-contained evaluation case.
///
/// Loaded from `case.toml` via [`TestCase::load_dir`]. The runner copies
/// the sibling `workspace/` directory into a tempdir for each run so cases
/// are completely isolated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestCase {
    /// Short, kebab-case identifier (matches the directory name).
    pub name: String,
    /// One-line human description.
    pub description: String,
    /// Language hint, e.g. `"rust"`. Used by language-specific evaluators
    /// to decide whether to run.
    pub language: String,
    /// The prompt fed to the worker via ACP `prompt`.
    pub prompt: String,
    /// Names of evaluators to run for this case. See
    /// [`crate::evaluators::evaluator_by_name`].
    pub evaluators: Vec<String>,
    /// Regex patterns the produced diff must match (used by the
    /// `pattern_match` evaluator). At least one must match.
    #[serde(default)]
    pub expected_patterns: Vec<String>,
    /// What the mock Ollama returns in mock mode. Ignored in live mode.
    pub mock_response: MockResponse,
    /// Difficulty tier, used by the `--difficulty` filter:
    /// - `"L1"` — saturated single-edit tasks (every modern coder model passes)
    /// - `"L2"` — multi-step, single-domain reasoning
    /// - `"L3"` — cross-domain / long-context (future)
    ///
    /// Defaults to `"L1"` so pre-existing cases need no change.
    #[serde(default = "default_difficulty")]
    pub difficulty: String,

    // ── Not serialized; populated by the loader ─────────────────────
    /// Absolute path to the case directory (the parent of `case.toml`).
    #[serde(skip)]
    pub case_dir: PathBuf,
}

/// Canned response body for mock mode.
///
/// `content` is what the worker would otherwise see from the LLM — for
/// our coding cases this is always a unified diff. The mock server wraps
/// it in `{ "message": { "content": "..." } }` to mimic Ollama's `/api/chat`
/// schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockResponse {
    pub content: String,
}

impl TestCase {
    /// Load a `TestCase` from `dir/case.toml`. The `workspace/` sibling
    /// directory is expected to exist (it's what the runner copies into
    /// the tempdir at run time).
    pub fn load_dir(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        let toml_path = dir.join("case.toml");
        if !toml_path.exists() {
            anyhow::bail!("missing case.toml in {}", dir.display());
        }
        let text = std::fs::read_to_string(&toml_path)?;
        let mut case: Self = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", toml_path.display()))?;
        case.case_dir = dir.to_path_buf();
        if !case.workspace_fixture().exists() {
            anyhow::bail!(
                "case {} declares no workspace fixture at {}",
                case.name,
                case.workspace_fixture().display()
            );
        }
        Ok(case)
    }

    /// Path to the workspace fixture (the case's `workspace/` subdir).
    pub fn workspace_fixture(&self) -> PathBuf {
        self.case_dir.join("workspace")
    }

    /// True if the case targets Rust source code.
    pub fn is_rust(&self) -> bool {
        self.language.eq_ignore_ascii_case("rust")
    }
}

/// Load every case under `cases_dir/` whose directory contains a
/// `case.toml`. Results are sorted by case name.
pub fn load_all(cases_dir: impl AsRef<Path>) -> anyhow::Result<Vec<TestCase>> {
    let cases_dir = cases_dir.as_ref();
    if !cases_dir.exists() {
        anyhow::bail!("cases dir does not exist: {}", cases_dir.display());
    }
    let mut cases = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(cases_dir)?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("case.toml").exists() {
            continue;
        }
        cases.push(TestCase::load_dir(&path)?);
    }
    Ok(cases)
}

/// Default difficulty tier for cases that don't declare one.
fn default_difficulty() -> String {
    "L1".to_string()
}

/// Keep only cases whose `difficulty` is in `wanted` (case-insensitive).
/// An empty `wanted` means "all tiers" — the default when `--difficulty`
/// is not passed.
pub fn filter_by_difficulty(cases: Vec<TestCase>, wanted: &[String]) -> Vec<TestCase> {
    if wanted.is_empty() {
        return cases;
    }
    cases
        .into_iter()
        .filter(|c| wanted.iter().any(|w| w.eq_ignore_ascii_case(&c.difficulty)))
        .collect()
}

/// Return the conventional cases directory:
/// `<CARGO_MANIFEST_DIR>/cases`.
pub fn default_cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cases")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_case(dir: &Path, name: &str, toml_body: &str) -> PathBuf {
        let case_dir = dir.join(name);
        std::fs::create_dir_all(case_dir.join("workspace")).unwrap();
        std::fs::write(case_dir.join("case.toml"), toml_body).unwrap();
        case_dir
    }

    #[test]
    fn load_dir_parses_toml_and_sets_case_dir() {
        let tmp = tempdir().unwrap();
        let toml = r#"
name = "demo"
description = "demo case"
language = "rust"
prompt = "do a thing"
evaluators = ["diff_nonempty", "pattern_match"]
expected_patterns = ["hello"]

[mock_response]
content = "diff goes here"
"#;
        let case_dir = write_case(tmp.path(), "001-demo", toml);
        let case = TestCase::load_dir(&case_dir).unwrap();
        assert_eq!(case.name, "demo");
        assert_eq!(case.language, "rust");
        assert!(case.is_rust());
        assert_eq!(case.evaluators, vec!["diff_nonempty", "pattern_match"]);
        assert_eq!(case.expected_patterns, vec!["hello"]);
        assert_eq!(case.mock_response.content, "diff goes here");
        assert_eq!(case.case_dir, case_dir);
        assert_eq!(case.workspace_fixture(), case_dir.join("workspace"));
    }

    #[test]
    fn load_dir_missing_toml_errors() {
        let tmp = tempdir().unwrap();
        let err = TestCase::load_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("missing case.toml"));
    }

    #[test]
    fn load_dir_missing_workspace_errors() {
        let tmp = tempdir().unwrap();
        let case_dir = tmp.path().join("bad");
        std::fs::create_dir_all(&case_dir).unwrap();
        std::fs::write(
            case_dir.join("case.toml"),
            r#"
name = "bad"
description = ""
language = "rust"
prompt = ""
evaluators = []

[mock_response]
content = ""
"#,
        )
        .unwrap();
        let err = TestCase::load_dir(&case_dir).unwrap_err();
        assert!(err.to_string().contains("workspace fixture"));
    }

    #[test]
    fn load_dir_bad_toml_errors() {
        let tmp = tempdir().unwrap();
        let case_dir = tmp.path().join("bad");
        std::fs::create_dir_all(case_dir.join("workspace")).unwrap();
        std::fs::write(case_dir.join("case.toml"), "not = valid = toml").unwrap();
        let err = TestCase::load_dir(&case_dir).unwrap_err();
        assert!(err.to_string().contains("parse"));
    }

    #[test]
    fn load_all_finds_cases_sorted() {
        let tmp = tempdir().unwrap();
        for n in ["002-bravo", "001-alpha"] {
            write_case(
                tmp.path(),
                n,
                &format!(
                    r#"
name = "{n}"
description = ""
language = "rust"
prompt = ""
evaluators = []

[mock_response]
content = ""
"#
                ),
            );
        }
        // A non-case directory should be skipped.
        std::fs::create_dir_all(tmp.path().join("not-a-case")).unwrap();

        let cases = load_all(tmp.path()).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name, "001-alpha");
        assert_eq!(cases[1].name, "002-bravo");
    }

    #[test]
    fn load_all_missing_dir_errors() {
        let err = load_all("/nonexistent/path/that/does/not/exist").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    /// The bundled cases under `newt-eval/cases/` must all parse and
    /// must declare only known evaluator names. This is the cheap
    /// regression catch for "you added a case but typo'd an evaluator".
    #[test]
    fn bundled_cases_load_and_reference_known_evaluators() {
        let dir = default_cases_dir();
        if !dir.exists() {
            // Possible during a fresh `cargo test` before the cases
            // directory has been created. Treated as a soft-pass so this
            // test doesn't fail in unrelated scaffolds.
            return;
        }
        let cases = load_all(&dir).expect("bundled cases should load");
        assert!(!cases.is_empty(), "expected at least one bundled case");
        for case in &cases {
            assert!(
                !case.evaluators.is_empty(),
                "{} has no evaluators",
                case.name
            );
            for ev in &case.evaluators {
                assert!(
                    crate::evaluators::evaluator_by_name(ev).is_some(),
                    "case {} references unknown evaluator '{}'",
                    case.name,
                    ev
                );
            }
        }
    }

    #[test]
    fn is_rust_case_insensitive() {
        let mut c = TestCase {
            name: "x".into(),
            description: "".into(),
            language: "Rust".into(),
            prompt: "".into(),
            evaluators: vec![],
            expected_patterns: vec![],
            mock_response: MockResponse { content: "".into() },
            difficulty: "L1".into(),
            case_dir: PathBuf::new(),
        };
        assert!(c.is_rust());
        c.language = "python".into();
        assert!(!c.is_rust());
    }

    #[test]
    fn difficulty_defaults_to_l1_when_absent() {
        let tmp = tempdir().unwrap();
        let case_dir = write_case(
            tmp.path(),
            "001-demo",
            r#"
name = "demo"
description = ""
language = "rust"
prompt = ""
evaluators = ["diff_nonempty"]

[mock_response]
content = ""
"#,
        );
        let case = TestCase::load_dir(&case_dir).unwrap();
        assert_eq!(
            case.difficulty, "L1",
            "missing difficulty must default to L1"
        );
    }

    #[test]
    fn filter_by_difficulty_selects_requested_tiers() {
        let mk = |name: &str, diff: &str| TestCase {
            name: name.into(),
            description: "".into(),
            language: "rust".into(),
            prompt: "".into(),
            evaluators: vec![],
            expected_patterns: vec![],
            mock_response: MockResponse { content: "".into() },
            difficulty: diff.into(),
            case_dir: PathBuf::new(),
        };
        let cases = vec![mk("a", "L1"), mk("b", "L2"), mk("c", "L1"), mk("d", "L3")];

        // Empty filter => all tiers.
        assert_eq!(filter_by_difficulty(cases.clone(), &[]).len(), 4);
        // Single tier (case-insensitive).
        let l2 = filter_by_difficulty(cases.clone(), &["l2".to_string()]);
        assert_eq!(l2.len(), 1);
        assert_eq!(l2[0].name, "b");
        // Multiple tiers.
        let l2l3 = filter_by_difficulty(cases, &["L2".to_string(), "L3".to_string()]);
        assert_eq!(l2l3.len(), 2);
    }
}
