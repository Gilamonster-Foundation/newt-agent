//! Newt-Agent tool surface.
//!
//! vi-minimal v0 set: `read`, `edit`, `search`, `apply_patch`.
//! Thin wrappers — when `thoon-fileops` publishes, this crate delegates
//! to it rather than reimplementing.

use std::path::Path;

pub fn read(_path: &Path) -> anyhow::Result<String> {
    anyhow::bail!("newt-tools::read not yet implemented")
}

pub fn edit(_path: &Path, _patch: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tools::edit not yet implemented")
}

pub fn search(_query: &str, _root: &Path) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("newt-tools::search not yet implemented")
}

pub fn apply_patch(_root: &Path, _diff: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tools::apply_patch not yet implemented")
}
