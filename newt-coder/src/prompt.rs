//! Prompt builder — stubbed in commit 1, real implementation in commit 2.

use std::path::{Path, PathBuf};

use crate::error::Result;

pub const DEFAULT_CONTEXT_CAP_CHARS: usize = 32_000;

pub const WHOLE_FILE_SYSTEM_PROMPT: &str = "\
You are a coding assistant editing files. \
For each file you change, emit ONLY the complete updated file contents. \
Do not include diffs, code fences, prose, or explanations. \
Start each file with a single line:  FILE: <relative path>\n\
Then the verbatim updated file contents, followed by a line containing only END-FILE. \
If you do not change a file, do not emit it. \
Do not invent files that don't exist.\
";

#[derive(Debug, Clone)]
pub struct CoderPrompt {
    pub system: String,
    pub user: String,
    pub included_files: Vec<PathBuf>,
}

pub fn build_prompt(_workspace: &Path, task: &str) -> Result<CoderPrompt> {
    // Placeholder — full implementation lands in commit 2.
    Ok(CoderPrompt {
        system: WHOLE_FILE_SYSTEM_PROMPT.to_string(),
        user: task.to_string(),
        included_files: Vec::new(),
    })
}
