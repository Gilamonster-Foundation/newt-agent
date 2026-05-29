//! Workspace scan — stubbed in commit 1, real implementation in commit 2.

use std::path::{Path, PathBuf};

use crate::error::Result;

pub fn scan_workspace_for_files(_workspace: &Path, _task: &str) -> Result<Vec<PathBuf>> {
    Ok(Vec::new())
}
