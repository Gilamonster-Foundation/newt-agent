//! Build the LLM prompt for a coder task.
//!
//! Strategy (per the failure-mode taxonomy in
//! `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`):
//!
//! 1. Scan the workspace for files the task references (or every
//!    source file in small workspaces — see `workspace_scan`).
//! 2. Inject each file's verbatim contents into the user message
//!    under `FILE: <path>` / `END-FILE` separators.
//! 3. Ask the model to emit the **complete updated file contents**,
//!    not a diff — the S5 strategy that won the bake-off
//!    (`golden_match=true` on qwen3-coder:30b, 224 tokens, 5.6 s).
//!
//! Total file context is capped at `DEFAULT_CONTEXT_CAP_CHARS`
//! (~8K tokens) to keep prompts within local-model context budgets.
//! When the cap is reached we drop the remaining files and emit a
//! `tracing::warn!` — the operator should see this and either narrow
//! the task or split it.

use std::path::{Path, PathBuf};

use crate::error::{CoderError, Result};
use crate::workspace_scan::scan_workspace_for_files;

/// Maximum total chars of file context to inject into a single prompt.
/// ~8K tokens at ~4 chars/token (English-ish average). Bake-off case
/// 001-rename-function used 224 tokens of context; this leaves a wide
/// margin for multi-file refactors before the prompt becomes
/// prohibitively expensive on local hardware.
pub const DEFAULT_CONTEXT_CAP_CHARS: usize = 32_000;

/// The S5 system prompt — whole-file emit, no diffs, no fences.
///
/// Pinning the exact wording matters: the bake-off (wf_ecc784ea-aa2)
/// showed that subtle changes ("emit ONLY" vs "respond with") flip
/// qwen3-coder between perfect rewrite and prose-with-fence. Don't
/// edit casually; if you change it, re-run the strategy bake-off.
pub const WHOLE_FILE_SYSTEM_PROMPT: &str = "\
You are a coding assistant editing files. \
For each file you change, emit ONLY the complete updated file contents. \
Do not include diffs, code fences, prose, or explanations. \
Start each file with a single line:  FILE: <relative path>\n\
Then the verbatim updated file contents, followed by a line containing only END-FILE. \
If you do not change a file, do not emit it. \
Do not invent files that don't exist.\
";

/// A built prompt — `system` + `user` strings ready for
/// `ChatRequest`, plus the list of files actually injected (useful
/// for audit logs and the foreman's scorecard).
#[derive(Debug, Clone)]
pub struct CoderPrompt {
    pub system: String,
    pub user: String,
    pub included_files: Vec<PathBuf>,
}

/// Build a prompt for `task` against `workspace`. The workspace is
/// scanned for relevant source files (see `scan_workspace_for_files`)
/// and their contents are injected verbatim into the user message
/// under `FILE:`/`END-FILE` separators.
pub fn build_prompt(workspace: &Path, task: &str) -> Result<CoderPrompt> {
    let files = scan_workspace_for_files(workspace, task)?;
    let (file_block, included) = render_files_block(workspace, &files, DEFAULT_CONTEXT_CAP_CHARS)?;

    let user = format!(
        "Task:\n{}\n\nFiles in the workspace (verbatim contents):\n{}",
        task.trim(),
        file_block,
    );

    Ok(CoderPrompt {
        system: WHOLE_FILE_SYSTEM_PROMPT.to_string(),
        user,
        included_files: included,
    })
}

/// Render the `FILE:` / `END-FILE` block for `files`, dropping any
/// file that would push the block past `cap_chars` and logging a
/// warning. Returns the rendered block + the subset actually
/// included.
fn render_files_block(
    workspace: &Path,
    files: &[PathBuf],
    cap_chars: usize,
) -> Result<(String, Vec<PathBuf>)> {
    let mut out = String::new();
    let mut included = Vec::new();
    for path in files {
        let abs = workspace.join(path);
        let content = std::fs::read_to_string(&abs)
            .map_err(|e| CoderError::Workspace(format!("read {}: {e}", abs.display())))?;
        let block = format!("FILE: {}\n{}\nEND-FILE\n\n", path.display(), content);
        if out.len() + block.len() > cap_chars {
            tracing::warn!(
                included = included.len(),
                total = files.len(),
                cap = cap_chars,
                "newt-coder context cap reached; dropping remaining files"
            );
            break;
        }
        out.push_str(&block);
        included.push(path.clone());
    }
    Ok((out, included))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(abs, contents).unwrap();
    }

    #[test]
    fn build_prompt_injects_mentioned_file_contents() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn greet() {}\n");
        write(tmp.path(), "src/unused.rs", "pub fn other() {}\n");

        let p = build_prompt(tmp.path(), "Rename greet to hello in src/lib.rs").unwrap();
        assert_eq!(p.system, WHOLE_FILE_SYSTEM_PROMPT);
        assert!(p.user.contains("FILE: src/lib.rs"));
        assert!(p.user.contains("pub fn greet() {}"));
        assert!(p.user.contains("END-FILE"));
        // Unmentioned file must not leak into the prompt.
        assert!(!p.user.contains("src/unused.rs"));
        assert_eq!(p.included_files.len(), 1);
    }

    #[test]
    fn build_prompt_includes_task_text_verbatim() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn x() {}\n");

        let task = "Add a panic to src/lib.rs";
        let p = build_prompt(tmp.path(), task).unwrap();
        assert!(p.user.contains(task));
    }

    #[test]
    fn render_files_block_caps_at_budget() {
        let tmp = TempDir::new().unwrap();
        // Three 100-char files. Cap at 250 so the third one drops.
        let body = "X".repeat(50);
        write(tmp.path(), "a.rs", &body);
        write(tmp.path(), "b.rs", &body);
        write(tmp.path(), "c.rs", &body);

        let files = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        let (block, included) = render_files_block(tmp.path(), &files, 200).unwrap();
        assert!(
            included.len() < files.len(),
            "cap should have dropped at least one file"
        );
        assert!(
            block.len() <= 200,
            "block {} bytes exceeded cap 200",
            block.len()
        );
    }

    #[test]
    fn render_files_block_propagates_read_error() {
        let tmp = TempDir::new().unwrap();
        let files = vec![PathBuf::from("does-not-exist.rs")];
        let err = render_files_block(tmp.path(), &files, 1000).unwrap_err();
        assert!(matches!(err, CoderError::Workspace(_)));
    }

    #[test]
    fn whole_file_system_prompt_pins_directive() {
        // Regression: the exact wording of the directive is load-bearing
        // (per the bake-off card). Pin it so a casual edit fails CI.
        assert!(WHOLE_FILE_SYSTEM_PROMPT.contains("ONLY the complete updated file contents"));
        assert!(WHOLE_FILE_SYSTEM_PROMPT.contains("FILE: <relative path>"));
        assert!(WHOLE_FILE_SYSTEM_PROMPT.contains("END-FILE"));
        assert!(WHOLE_FILE_SYSTEM_PROMPT.contains("Do not include diffs"));
    }
}
