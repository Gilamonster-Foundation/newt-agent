//! One bounded, authority-preserving hint for file-tool I/O failures (#2755).
use std::path::{Component, Path, PathBuf};

use crate::caveats::Scope;

/// Error adapters currently erase io::Error into text. Confirm absence instead
/// of parsing OS-specific error prose; never decorate a capability refusal.
/// This only suggests a path: it neither grants access nor retries the tool.
pub(super) fn on_error(
    mut error: String,
    path: &str,
    workspace: &str,
    read: &Scope<String>,
) -> String {
    if !error.starts_with("error:") || path.len() > 4096 {
        return error;
    }
    let root = Path::new(workspace);
    let full = root.join(path);
    if !matches!(std::fs::metadata(&full), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
        return error;
    }
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
    let Ok(physical_root) = root.canonicalize() else {
        return error;
    };
    // No directory walks or filename search. At most 32 suffix candidates;
    // prefer the longest remaining suffix and emit only the first safe hit.
    for skip in 1..parts.len().min(33) {
        let relative: PathBuf = parts[skip..].iter().collect();
        let candidate = root.join(&relative);
        if !super::tui_permits_path(read, &candidate.to_string_lossy()) {
            continue;
        }
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
