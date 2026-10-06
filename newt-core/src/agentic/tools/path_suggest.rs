//! One bounded, authority-preserving hint for file-tool I/O failures (#2755).
use std::path::{Component, Path, PathBuf};

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
        let text = format!(
            "{}\nresolved against {}",
            self.text,
            crate::worktree_adoption::task_path_literal(Path::new(workspace))
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
