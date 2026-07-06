//! Run the Python verify scorers over an **arbitrary** workspace — the
//! ground-truth rig's measurement tool, decoupled from the case-fixture
//! framework.
//!
//! The #340 `python_imports` oracle is written against an [`EvalContext`]; here
//! we synthesize a minimal Python context so the **same** evaluator grades any
//! output tree against a declared surface (`<surface_dir>/python_surface.json`).
//! This is what the live rig (#75) calls to score real `newt`+model output: a
//! drive step (the gauntlet headless path, or later `newt run`) produces a
//! workspace, and `score_python_workspace` turns it into a verdict.

use std::path::Path;

use newt_acp_worker::TaskReply;

use crate::cases::{MockResponse, TestCase};
use crate::evaluators::{Evaluator, PythonImportsEvaluator};
use crate::scorecard::{EvalContext, EvalResult};

/// Score `workspace`'s Python output against the module surface declared in
/// `<surface_dir>/python_surface.json`, using the #339/#340 verify oracle.
///
/// Decoupled from the case framework: we build a synthetic Python
/// [`EvalContext`] (the `python_imports` evaluator reads only the case language,
/// the case dir, and the workspace — never the reply), so the same oracle runs
/// against any directory the live rig hands it.
///
/// # Errors
/// Propagates the (effectively infallible) placeholder-reply construction.
pub fn score_python_workspace(workspace: &Path, surface_dir: &Path) -> anyhow::Result<EvalResult> {
    let case = TestCase {
        name: "score".to_string(),
        description: "ad-hoc workspace score".to_string(),
        language: "python".to_string(),
        prompt: String::new(),
        evaluators: vec!["python_imports".to_string()],
        expected_patterns: Vec::new(),
        expected_output: None,
        output_match: None,
        mock_response: MockResponse {
            content: String::new(),
        },
        difficulty: "L1".to_string(),
        case_dir: surface_dir.to_path_buf(),
    };
    // `python_imports` ignores the reply; a valid placeholder fills the context.
    let reply = TaskReply::new("none", "", "", true)?;
    let ctx = EvalContext {
        case,
        workspace: workspace.to_path_buf(),
        baseline: workspace.to_path_buf(),
        reply,
    };
    Ok(PythonImportsEvaluator.evaluate(&ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn scores_a_workspace_and_catches_the_incident() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The declared real surface (umbrella `newt_agent` with submodules).
        write_file(
            &root.join("python_surface.json"),
            r#"{"modules": ["newt_agent.core", "newt_agent.data"]}"#,
        );
        // The model's (fabricated) output, mirroring the incident.
        write_file(
            &root.join("examples/ex.py"),
            "from newt_core import classify\nfrom newt_agent.core import Router\nimport os\n",
        );
        let result = score_python_workspace(root, root).unwrap();
        assert!(!result.passed, "a fabricated import must fail the score");
        assert!(
            result.details.contains("newt_core"),
            "details: {}",
            result.details
        );
    }

    #[test]
    fn clean_workspace_passes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(
            &root.join("python_surface.json"),
            r#"{"modules": ["newt_agent.core"]}"#,
        );
        write_file(
            &root.join("ok.py"),
            "from newt_agent.core import Router\nimport json\n",
        );
        let result = score_python_workspace(root, root).unwrap();
        assert!(result.passed, "details: {}", result.details);
    }
}
