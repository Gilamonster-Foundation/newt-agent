//! The `knowledge_base` technique generalized beyond PyO3 (#669): a workspace
//! **API-surface** provider.
//!
//! Where [`crate::FfiSurfaceProvider`] injects the PyO3 import paths, this injects
//! the workspace's authoritative PUBLIC SYMBOL surface — `pub fn`/`struct`/`enum`/
//! `trait` (Rust) and top-level non-`_` `def`/`class` (Python) — into the frozen
//! system prompt. It rides the same provider seam, gated by the same
//! `knowledge_base` technique, so a non-PyO3 workspace still gets a stable base.
//!
//! **Compression role (#661 group E).** The block lives in the compressor's
//! protected head ([`super::agentic::compress`] `head_len`), so it is NEVER
//! summarized: the model grounds against real names even after the middle is
//! compacted, and an API detail can't be lost to compression.
//!
//! Bounded ([`MAX_BLOCK_CHARS`]) and a no-op on a workspace with no public symbols.

use async_trait::async_trait;
use regex::Regex;

use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;
use crate::symbols::Lang;

/// Hard ceiling on the rendered surface block — it rides every turn's system
/// prompt, so it must stay small (the stable-base vs. context-cost tradeoff).
const MAX_BLOCK_CHARS: usize = 3_000;
/// Per-file symbol cap, so one huge module can't crowd out the rest of the surface.
const MAX_SYMBOLS_PER_FILE: usize = 12;

/// Injects the workspace's public API surface into the system prompt.
#[derive(Default)]
pub struct ApiSurfaceProvider {
    block: Option<String>,
}

impl ApiSurfaceProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// API entry-point files expose the public surface — list them before the rest.
fn is_entry_point(path: &str) -> bool {
    ["/lib.rs", "/mod.rs", "/main.rs", "/__init__.py"]
        .iter()
        .any(|s| path.ends_with(s))
        || path == "lib.rs"
        || path == "main.rs"
        || path == "__init__.py"
}

/// The public (exported) symbols a source file declares — bare-`pub` items in
/// Rust, non-`_` top-level `def`/`class` in Python. Private/internal symbols are
/// excluded: this is the API the model should ground against, not the impl.
fn public_symbols(content: &str, lang: Lang) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    match lang {
        Lang::Rust => {
            // Bare `pub` only (not `pub(crate)` / `pub(in …)`): the genuine public
            // surface. Mirrors the keyword set the symbols.rs extractor uses.
            let re = Regex::new(
                r"^\s*pub\s+(?:async\s+|const\s+|unsafe\s+|default\s+)*(fn|struct|enum|trait)\s+(\w+)",
            )
            .expect("static regex compiles");
            for line in content.lines() {
                if let Some(c) = re.captures(line) {
                    out.push((c[2].to_string(), kind_label(&c[1])));
                }
            }
        }
        Lang::Python => {
            // Module-level (column 0) `def`/`class` with a public (non-`_`) name.
            for line in content.lines() {
                for (kw, label) in [("def ", "fn"), ("class ", "class")] {
                    if let Some(rest) = line.strip_prefix(kw) {
                        let name: String = rest
                            .chars()
                            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                            .collect();
                        if !name.is_empty() && !name.starts_with('_') {
                            out.push((name, label));
                        }
                        break;
                    }
                }
            }
        }
    }
    out
}

fn kind_label(kw: &str) -> &'static str {
    match kw {
        "fn" => "fn",
        "struct" => "struct",
        "enum" => "enum",
        _ => "trait",
    }
}

/// Render a bounded public-symbol surface from already-gathered `(path, content)`
/// files. Pure (no filesystem) so the ordering + bounding logic is unit-testable.
/// `None` when nothing public is found — a no-op, like the FFI provider on a
/// non-PyO3 workspace.
fn render_from_files(files: &[(String, String)]) -> Option<String> {
    let mut entries: Vec<(&str, Vec<(String, &'static str)>)> = Vec::new();
    for (path, content) in files {
        let Some(lang) = Lang::from_path(path) else {
            continue;
        };
        let mut syms = public_symbols(content, lang);
        if syms.is_empty() {
            continue;
        }
        syms.truncate(MAX_SYMBOLS_PER_FILE);
        entries.push((path.as_str(), syms));
    }
    if entries.is_empty() {
        return None;
    }
    // Entry-point files first (the API), then alphabetical — deterministic output.
    entries.sort_by_key(|(p, _)| (!is_entry_point(p), *p));

    let mut out = String::from(
        "[WORKSPACE API SURFACE — authoritative public symbols defined in this \
         workspace. Use these EXACT names; do not invent APIs. read_file a path for \
         signatures.]\n",
    );
    for (path, syms) in entries {
        if out.len() >= MAX_BLOCK_CHARS {
            out.push_str("- … (surface truncated to fit the budget)\n");
            break;
        }
        let rendered: Vec<String> = syms
            .iter()
            .map(|(name, kind)| format!("{name} ({kind})"))
            .collect();
        out.push_str("- ");
        out.push_str(path);
        out.push_str(": ");
        out.push_str(&rendered.join(", "));
        out.push('\n');
    }
    Some(out)
}

#[async_trait]
impl MemoryProvider for ApiSurfaceProvider {
    fn name(&self) -> &str {
        "api_surface"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        // `gather_code_files` is the only fs touch; rendering is pure (tested).
        // Strip the workspace prefix so paths render relative + read_file-able.
        let files: Vec<(String, String)> = crate::gather_code_files(&ctx.workspace)
            .into_iter()
            .map(|(path, content)| {
                let rel = path
                    .strip_prefix(&ctx.workspace)
                    .unwrap_or(&path)
                    .trim_start_matches('/')
                    .to_string();
                (rel, content)
            })
            .collect();
        self.block = render_from_files(&files);
        if self.block.is_some() {
            tracing::info!("knowledge_base: workspace API surface injected");
        }
        Ok(())
    }

    fn system_prompt_block(&self) -> Option<String> {
        self.block.clone()
    }

    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        Vec::new()
    }

    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_public_symbols_exclude_private_and_pub_crate() {
        let src = "pub fn open(p: &str) {}\n\
                   pub struct Router;\n\
                   pub(crate) fn internal() {}\n\
                   fn private() {}\n\
                   pub enum Tier { A }\n\
                   pub async fn run() {}\n";
        let got = public_symbols(src, Lang::Rust);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"open") && names.contains(&"Router"));
        assert!(
            names.contains(&"Tier") && names.contains(&"run"),
            "pub async fn"
        );
        assert!(
            !names.contains(&"internal"),
            "pub(crate) is internal, not the API"
        );
        assert!(!names.contains(&"private"), "private fn excluded");
    }

    #[test]
    fn python_public_top_level_defs_and_classes() {
        let src = "def public_fn():\n    pass\n\
                   def _private():\n    pass\n\
                   class MyClass:\n    def method(self):\n        pass\n";
        let got = public_symbols(src, Lang::Python);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"public_fn") && names.contains(&"MyClass"));
        assert!(
            !names.contains(&"_private"),
            "underscore-prefixed is private"
        );
        assert!(
            !names.contains(&"method"),
            "indented (method) is not module-level"
        );
    }

    #[test]
    fn render_orders_entry_points_first_and_labels_kinds() {
        let files = vec![
            (
                "newt-core/src/router.rs".to_string(),
                "pub struct Router;".to_string(),
            ),
            (
                "newt-core/src/lib.rs".to_string(),
                "pub fn boot() {}".to_string(),
            ),
        ];
        let block = render_from_files(&files).expect("a surface");
        assert!(block.contains("WORKSPACE API SURFACE"));
        assert!(block.contains("Router (struct)") && block.contains("boot (fn)"));
        // lib.rs (entry point) is listed before router.rs.
        let lib = block.find("lib.rs").unwrap();
        let router = block.find("router.rs").unwrap();
        assert!(lib < router, "entry-point files come first");
    }

    #[test]
    fn render_is_a_noop_when_nothing_public() {
        let files = vec![
            ("a.rs".to_string(), "fn private() {}".to_string()),
            ("notes.md".to_string(), "# not code".to_string()),
        ];
        assert!(render_from_files(&files).is_none());
    }

    #[test]
    fn render_is_bounded() {
        // Many files, each with symbols — the block must respect MAX_BLOCK_CHARS.
        let files: Vec<(String, String)> = (0..500)
            .map(|i| {
                (
                    format!("crate{i}/src/file.rs"),
                    format!("pub fn f{i}() {{}}"),
                )
            })
            .collect();
        let block = render_from_files(&files).expect("a surface");
        assert!(
            block.len() <= MAX_BLOCK_CHARS + 80,
            "bounded: {} chars",
            block.len()
        );
        assert!(block.contains("surface truncated"), "names the truncation");
    }
}
