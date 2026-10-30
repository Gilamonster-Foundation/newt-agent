//! Model fs paths: the one resolution every file tool uses (#3039), and one
//! bounded, authority-preserving hint for file-tool I/O failures (#2755).
use std::path::{Component, Path, PathBuf};

/// #3039: where a model-written fs `path` points. A leading `~` or `~/` is the
/// home directory ([`crate::config::home_dir`], #1458) — a model's first
/// instinct for home files — never a `<workspace>/~` literal. The result is an
/// ordinary absolute path, so every fs gate judges it exactly as if the model
/// had typed it out: expansion grants nothing. `~user` is refused, not looked
/// up. Anything else joins the workspace, as before.
pub(crate) fn resolve_tool_path(workspace: &Path, path: &str) -> Result<PathBuf, String> {
    let Some(rest) = path.strip_prefix('~') else {
        return Ok(workspace.join(path));
    };
    if !rest.is_empty() && !rest.starts_with(std::path::is_separator) {
        return Err(format!(
            "error: `{path}`: only a leading `~` or `~/` expands (to your home \
             directory); `~user` forms are not expanded (#3039). Use the absolute path."
        ));
    }
    // Every separator after `~` goes (`~//x` is `~/x`). A rest that still
    // carries a root or prefix (Windows `C:`) would make `join` REPLACE home
    // and act on another file, so it is refused rather than joined.
    let rest = Path::new(rest.trim_start_matches(std::path::is_separator));
    if rest.has_root() || matches!(rest.components().next(), Some(Component::Prefix(_))) {
        return Err(format!(
            "error: `{path}`: the part after `~/` must be relative to your home directory"
        ));
    }
    let home = crate::config::home_dir().ok_or_else(|| {
        format!("error: `{path}`: no home directory is set to expand `~`; use the absolute path")
    })?;
    Ok(home.join(rest))
}

/// #3045 review: the provenance locator of a model-written path — resolved
/// like the tool resolved it, then relative to the workspace when inside it,
/// so `~/project/f` under a workspace at `~/project` records `f`. A target
/// outside keeps its absolute spelling, which the artifact layer refuses as
/// it refuses any absolute locator; an unresolvable label is kept as written.
pub(crate) fn artifact_locator(workspace: &str, label: &str) -> String {
    let workspace = Path::new(workspace);
    resolve_tool_path(workspace, label).map_or_else(
        |_| label.to_owned(),
        |full| {
            full.strip_prefix(workspace)
                .unwrap_or(&full)
                .to_string_lossy()
                .into_owned()
        },
    )
}

use crate::caveats::Scope;

/// An ephemeral file-I/O failure retains the operation's kind and operand until
/// the model-facing boundary. Refusals have no I/O kind and cannot suggest.
#[derive(Clone, Debug)]
pub(super) struct FileIoError {
    kind: Option<std::io::ErrorKind>,
    operand: PathBuf,
    text: String,
}

impl FileIoError {
    pub(super) fn io(error: &std::io::Error, operand: &Path, text: String) -> Self {
        Self {
            kind: Some(error.kind()),
            operand: operand.into(),
            text,
        }
    }

    pub(super) fn render(self, workspace: &str, read: &Scope<String>) -> String {
        if self.kind != Some(std::io::ErrorKind::NotFound) {
            return self.text;
        }
        // #3039: the text names the path as written; this names where it
        // resolved (a relative operand is still workspace-joined).
        let text = format!(
            "{}\nresolved to {}",
            self.text,
            crate::worktree_adoption::task_path_literal(&Path::new(workspace).join(&self.operand))
        );
        let Some(path) = self.operand.to_str() else {
            return text;
        };
        suggest(text, path, workspace, read)
    }
}

impl From<String> for FileIoError {
    fn from(text: String) -> Self {
        Self {
            kind: None,
            operand: PathBuf::new(),
            text,
        }
    }
}

// Consumers without suggestion context retain the original refusal/error text.
impl From<FileIoError> for String {
    fn from(error: FileIoError) -> Self {
        error.text
    }
}

/// Never probe the failed operand: its original error already proves NotFound.
fn suggest(mut error: String, path: &str, workspace: &str, read: &Scope<String>) -> String {
    if path.len() > 4096 {
        return error;
    }
    let root = Path::new(workspace);
    let input = Path::new(path)
        .strip_prefix(root)
        .unwrap_or(Path::new(path));
    let mut parts = Vec::new();
    for component in input.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => return error,
            _ => {}
        }
        if parts.len() > 128 {
            return error;
        }
    }
    // No directory walks or filename search. At most 32 suffix candidates;
    // prefer the longest remaining suffix and emit only the first safe hit.
    for skip in 1..parts.len().min(33) {
        let relative: PathBuf = parts[skip..].iter().collect();
        let candidate = root.join(&relative);
        if !super::tui_permits_path(read, &candidate.to_string_lossy()) {
            continue;
        }
        let Ok(physical_root) = root.canonicalize() else {
            return error;
        };
        let Ok(physical) = candidate.canonicalize() else {
            continue;
        };
        if !physical.starts_with(&physical_root) {
            continue;
        }
        // A lexical grant must not expose a suffix through a symlink escaping
        // that grant. Canonicalize grants too (e.g. macOS /var -> /private/var).
        if let Scope::Only(roots) = read {
            if !roots.iter().any(|grant| {
                Path::new(grant)
                    .canonicalize()
                    .is_ok_and(|grant| physical.starts_with(grant))
            }) {
                continue;
            }
        }
        let label = parts[skip..]
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        error.push_str(&format!(
            "\ndid you mean {}?",
            super::file_capture::display_text(&label)
        ));
        break;
    }
    error
}
