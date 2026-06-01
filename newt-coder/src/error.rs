//! Error type for the newt-coder plugin.
//!
//! Mirrors the failure-mode taxonomy in
//! `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`:
//! workspace scan failure, prompt-too-large guard trip, malformed
//! emission, file-write failure, and inference-backend errors are
//! distinct variants so callers (and tests) can pattern-match.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoderError {
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("prompt too large: {actual} chars > cap {cap}")]
    PromptTooLarge { actual: usize, cap: usize },
    #[error("emission malformed: {0}")]
    BadEmission(String),
    #[error("file write failed: {0}")]
    FileWrite(String),
    /// The emitted body for `path` was empty or whitespace-only.
    ///
    /// A whole-file emission must contain something to write; an empty
    /// body is never a legitimate rewrite. The `file write failed:`
    /// prefix is load-bearing — the ACP worker and existing call sites
    /// match on it.
    #[error("file write failed: empty emission for '{path}'")]
    EmptyEmission { path: String },
    /// The emitted body for `path` looked like a unified diff rather
    /// than the complete file contents (its first non-blank line begins
    /// with `--- `, `+++ `, or `@@`).
    #[error("file write failed: emission for '{path}' looks like a diff, not a whole file")]
    LooksLikeDiff { path: String },
    /// The emitted body for `path` still began with a leaked `FILE:`
    /// marker as its first non-blank line (defense in depth in case the
    /// parser did not strip it).
    #[error("file write failed: emission for '{path}' still begins with a leaked FILE: marker")]
    LeakedMarker { path: String },
    #[error("inference: {0}")]
    Inference(String),
    /// The peer's signed [`Caveats`](newt_core::Caveats) deny the tool
    /// call this dispatch would have made.
    ///
    /// `kind` names the axis that refused — one of `"fs_read"`,
    /// `"fs_write"`, `"net"`, `"exec"`, or `"max_calls"` — and `target`
    /// names the concrete item the dispatch tried to touch (the path, the
    /// host, or `"<turn>"` for the `max_calls` budget). The pair is
    /// load-bearing for arbiter scorecards: every `CapabilityDenied`
    /// becomes a scrubbed-sortie reason, not a model failure.
    #[error("capability denied: {kind} does not permit '{target}'")]
    CapabilityDenied { kind: &'static str, target: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_error_renders() {
        let e = CoderError::Workspace("missing dir".to_string());
        assert!(e.to_string().contains("missing dir"));
    }

    #[test]
    fn prompt_too_large_renders() {
        let e = CoderError::PromptTooLarge {
            actual: 100,
            cap: 50,
        };
        let s = e.to_string();
        assert!(s.contains("100"));
        assert!(s.contains("50"));
    }

    #[test]
    fn bad_emission_renders() {
        let e = CoderError::BadEmission("no FILE: header".to_string());
        assert!(e.to_string().contains("no FILE: header"));
    }

    #[test]
    fn file_write_renders() {
        let e = CoderError::FileWrite("permission denied".to_string());
        assert!(e.to_string().contains("permission denied"));
    }

    #[test]
    fn empty_emission_renders_with_prefix() {
        let e = CoderError::EmptyEmission {
            path: "src/lib.rs".to_string(),
        };
        let s = e.to_string();
        assert!(s.starts_with("file write failed:"), "got: {s}");
        assert!(s.contains("src/lib.rs"));
    }

    #[test]
    fn looks_like_diff_renders_with_prefix() {
        let e = CoderError::LooksLikeDiff {
            path: "src/lib.rs".to_string(),
        };
        let s = e.to_string();
        assert!(s.starts_with("file write failed:"), "got: {s}");
        assert!(s.contains("diff"));
    }

    #[test]
    fn leaked_marker_renders_with_prefix() {
        let e = CoderError::LeakedMarker {
            path: "src/lib.rs".to_string(),
        };
        let s = e.to_string();
        assert!(s.starts_with("file write failed:"), "got: {s}");
        assert!(s.contains("FILE:"));
    }

    #[test]
    fn inference_renders() {
        let e = CoderError::Inference("backend offline".to_string());
        assert!(e.to_string().contains("backend offline"));
    }

    #[test]
    fn io_error_converts() {
        let io: std::io::Error = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
        let e: CoderError = io.into();
        assert!(matches!(e, CoderError::Io(_)));
    }

    #[test]
    fn capability_denied_renders_kind_and_target() {
        let e = CoderError::CapabilityDenied {
            kind: "fs_write",
            target: "forbidden.rs".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("capability denied"));
        assert!(s.contains("fs_write"));
        assert!(s.contains("forbidden.rs"));
    }

    #[test]
    fn capability_denied_kinds_match_dispatch_axes() {
        for kind in ["fs_read", "fs_write", "net", "exec", "max_calls"] {
            let e = CoderError::CapabilityDenied {
                kind,
                target: "x".to_string(),
            };
            assert!(e.to_string().contains(kind));
        }
    }
}
