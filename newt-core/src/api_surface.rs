//! The `knowledge_base` workspace **API-surface** technique (#669), with a
//! **pluggable language-pack** model.
//!
//! A language pack is pure DATA ([`crate::config::LanguagePack`]): file
//! extensions, entry-point file globs, and regex symbol-extraction rules with
//! free-form kind labels. So adding a language is *config, not code*.
//!
//! - **Built-in packs** (first-class): Rust, Python, Bash, C/C++, Go, Java —
//!   see [`builtin_packs`]. They double as the canonical examples.
//! - **External packs**: drop a `<name>.toml` into `~/.newt/language-packs/`
//!   (global) or `.newt/language-packs/` (project-local), or inline under
//!   `[[context.api_surface.language_packs]]`. Packs merge **by `name`**, so a
//!   community pack (Ruby, Swift, Objective-C, …) adds or overrides without ever
//!   touching the binary. See `docs/language-packs.md` + `examples/language-packs/`.
//!
//! The rendered surface rides the **frozen system prompt** — the compressor's
//! protected head ([`super::agentic::compress`] `head_len`) — so it is a stable
//! base that is never summarized (#661 group E).
//!
//! ## Extraction engine: regex is the BOOTSTRAP; AST (tree-sitter) is the target
//!
//! Per-line regex rules are a deliberate bootstrap — fragile for real code
//! (multi-line declarations, macros, generics, attributes). The **right** engine
//! is an AST parser (tree-sitter): a pack becomes a *grammar + a tree-sitter
//! query*, and the query — like the regex rules here — is pluggable DATA. The
//! architecture is chosen so that swap is local: `extensions` / `entry_points` /
//! merge-by-name / drop-in loading are all engine-agnostic; only the per-pack
//! extraction rules change shape (regex → query). Tracked as a follow-up.

use async_trait::async_trait;
use regex::Regex;
use std::path::Path;

use crate::config::{ApiSurfaceConfig, LanguagePack, SymbolRule};
use crate::memory::{MemMessage, MemoryProvider, SessionContext};
use crate::metrics::TurnMetrics;

fn rule(pattern: &str, kind: &str) -> SymbolRule {
    SymbolRule {
        pattern: pattern.to_string(),
        kind: kind.to_string(),
    }
}

/// The first-class built-in language packs (Rust, Python, Bash, C/C++, Go, Java).
/// These also serve as the worked examples a contributor copies — `newt
/// language-pack template <lang>` emits one to a drop-in file. Best-effort regexes
/// (a surface, not a parser); override any of them by name with your own pack.
#[must_use]
pub fn builtin_packs() -> Vec<LanguagePack> {
    vec![
        LanguagePack {
            name: "rust".into(),
            extensions: vec!["rs".into()],
            entry_points: vec!["lib.rs".into(), "mod.rs".into(), "main.rs".into()],
            symbols: vec![
                rule(
                    r"^\s*pub\s+(?:async\s+|const\s+|unsafe\s+|default\s+)*fn\s+(\w+)",
                    "fn",
                ),
                rule(r"^\s*pub\s+struct\s+(\w+)", "struct"),
                rule(r"^\s*pub\s+enum\s+(\w+)", "enum"),
                rule(r"^\s*pub\s+(?:unsafe\s+)?trait\s+(\w+)", "trait"),
                rule(r"^\s*pub\s+mod\s+(\w+)", "mod"),
            ],
        },
        LanguagePack {
            name: "python".into(),
            extensions: vec!["py".into()],
            entry_points: vec!["__init__.py".into(), "__main__.py".into()],
            // Module-level (column 0), public (non-`_`) names.
            symbols: vec![
                rule(r"^def\s+([a-zA-Z]\w*)", "fn"),
                rule(r"^class\s+([a-zA-Z]\w*)", "class"),
            ],
        },
        LanguagePack {
            name: "bash".into(),
            extensions: vec!["sh".into(), "bash".into()],
            entry_points: vec![],
            symbols: vec![
                rule(r"^function\s+([a-zA-Z_]\w*)", "fn"),
                rule(r"^([a-zA-Z_]\w*)\s*\(\s*\)", "fn"),
            ],
        },
        LanguagePack {
            name: "c_cpp".into(),
            extensions: vec![
                "c".into(),
                "cc".into(),
                "cpp".into(),
                "cxx".into(),
                "h".into(),
                "hpp".into(),
                "hh".into(),
                "hxx".into(),
            ],
            // Headers are the public surface — list them first.
            entry_points: vec!["*.h".into(), "*.hpp".into(), "*.hh".into(), "*.hxx".into()],
            symbols: vec![
                rule(r"^\s*(?:typedef\s+)?struct\s+(\w+)", "struct"),
                rule(r"^\s*class\s+(\w+)", "class"),
                rule(r"^\s*enum\s+(?:class\s+)?(\w+)", "enum"),
                // Rough free-function declaration: `<type> name(`.
                rule(r"^[A-Za-z_][\w\s\*:<>]*\s+(\w+)\s*\(", "fn"),
            ],
        },
        LanguagePack {
            name: "go".into(),
            extensions: vec!["go".into()],
            entry_points: vec!["doc.go".into()],
            // Go exports = capitalized identifiers.
            symbols: vec![
                rule(r"^func\s+(?:\([^)]*\)\s*)?([A-Z]\w*)", "func"),
                rule(r"^type\s+([A-Z]\w*)\s+struct", "struct"),
                rule(r"^type\s+([A-Z]\w*)\s+interface", "interface"),
                rule(r"^type\s+([A-Z]\w*)", "type"),
            ],
        },
        LanguagePack {
            name: "java".into(),
            extensions: vec!["java".into()],
            entry_points: vec!["package-info.java".into()],
            symbols: vec![
                rule(
                    r"^\s*public\s+(?:final\s+|abstract\s+)*class\s+(\w+)",
                    "class",
                ),
                rule(r"^\s*public\s+(?:final\s+)?interface\s+(\w+)", "interface"),
                rule(
                    r"^\s*public\s+(?:static\s+)?(?:final\s+)?enum\s+(\w+)",
                    "enum",
                ),
                rule(
                    r"^\s*public\s+(?:static\s+|final\s+|abstract\s+|synchronized\s+)*[\w<>\[\],\s]+\s+(\w+)\s*\(",
                    "method",
                ),
            ],
        },
    ]
}

/// Load language packs from a drop-in directory (`<dir>/*.toml`, one pack per
/// file). A malformed pack file is **skipped with a warning**, never fatal — the
/// same "drop-in, tolerant" contract as the `[backends]` directory. A missing
/// directory yields `[]`.
#[must_use]
pub fn load_packs_from_dir(dir: &Path) -> Vec<LanguagePack> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path).map(|s| toml::from_str::<LanguagePack>(&s)) {
            Ok(Ok(pack)) => out.push(pack),
            Ok(Err(e)) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping malformed language pack");
            }
            Err(_) => {}
        }
    }
    out
}

/// Merge pack layers **by name** — later layers win (built-ins < global dir <
/// project dir < inline config). A custom pack named `rust` replaces the built-in
/// `rust`; a new name adds a language. Output order is stable (insertion order of
/// first appearance, then last value wins).
#[must_use]
pub fn merge_packs(layers: Vec<Vec<LanguagePack>>) -> Vec<LanguagePack> {
    // Preserve first-seen order while letting later layers overwrite the value.
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, LanguagePack> =
        std::collections::HashMap::new();
    for layer in layers {
        for pack in layer {
            if !by_name.contains_key(&pack.name) {
                order.push(pack.name.clone());
            }
            by_name.insert(pack.name.clone(), pack);
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

/// A [`LanguagePack`] with its symbol rules compiled. Invalid regexes are dropped
/// (with a warning) so one bad rule can't disable the surface.
struct CompiledPack {
    extensions: Vec<String>,
    entry_points: Vec<String>,
    rules: Vec<(Regex, String)>,
}

fn compile(packs: Vec<LanguagePack>) -> Vec<CompiledPack> {
    packs
        .into_iter()
        .map(|p| {
            let rules = p
                .symbols
                .into_iter()
                .filter_map(|r| match Regex::new(&r.pattern) {
                    Ok(re) => Some((re, r.kind)),
                    Err(e) => {
                        tracing::warn!(pack = %p.name, pattern = %r.pattern, error = %e, "skipping invalid symbol rule");
                        None
                    }
                })
                .collect();
            CompiledPack {
                extensions: p.extensions,
                entry_points: p.entry_points,
                rules,
            }
        })
        .collect()
}

/// Tiny filename glob: `*` (any), `*.ext` (suffix), else exact match.
fn glob_match(pattern: &str, filename: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        filename.ends_with(suffix)
    } else {
        pattern == filename
    }
}

/// Injects the workspace's public API surface into the system prompt, driven by a
/// resolved set of language packs.
pub struct ApiSurfaceProvider {
    packs: Vec<CompiledPack>,
    max_block_chars: usize,
    max_symbols_per_file: usize,
    block: Option<String>,
}

impl ApiSurfaceProvider {
    /// Construct from already-resolved packs (built-ins + drop-in dirs + inline,
    /// merged by the caller via [`merge_packs`]) and the surface budget.
    #[must_use]
    pub fn new(packs: Vec<LanguagePack>, cfg: &ApiSurfaceConfig) -> Self {
        Self {
            packs: compile(packs),
            max_block_chars: cfg.max_block_chars,
            max_symbols_per_file: cfg.max_symbols_per_file,
            block: None,
        }
    }

    /// Convenience for the common case: built-in packs + inline config packs (no
    /// drop-in dirs). The TUI uses [`new`](Self::new) with the dir layers added.
    #[must_use]
    pub fn from_config(cfg: &ApiSurfaceConfig) -> Self {
        let packs = merge_packs(vec![builtin_packs(), cfg.language_packs.clone()]);
        Self::new(packs, cfg)
    }

    fn pack_for(&self, path: &str) -> Option<&CompiledPack> {
        let ext = path.rsplit('.').next().unwrap_or("");
        self.packs
            .iter()
            .find(|p| p.extensions.iter().any(|e| e == ext))
    }

    fn public_symbols(&self, path: &str, content: &str) -> Vec<(String, String)> {
        let Some(pack) = self.pack_for(path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in content.lines() {
            for (re, kind) in &pack.rules {
                if let Some(name) = re.captures(line).and_then(|c| c.get(1)) {
                    out.push((name.as_str().to_string(), kind.clone()));
                    break; // first matching rule wins for a line
                }
            }
        }
        out
    }

    fn is_entry_point(&self, path: &str) -> bool {
        let Some(pack) = self.pack_for(path) else {
            return false;
        };
        let filename = path.rsplit('/').next().unwrap_or(path);
        pack.entry_points.iter().any(|g| glob_match(g, filename))
    }

    /// Render a bounded surface from already-gathered `(path, content)` files.
    /// Pure (no filesystem) so ordering + bounding + extraction are unit-testable.
    fn render(&self, files: &[(String, String)]) -> Option<String> {
        let mut entries: Vec<(&str, Vec<(String, String)>)> = Vec::new();
        for (path, content) in files {
            let mut syms = self.public_symbols(path, content);
            if syms.is_empty() {
                continue;
            }
            syms.truncate(self.max_symbols_per_file);
            entries.push((path.as_str(), syms));
        }
        if entries.is_empty() {
            return None;
        }
        // Entry-point files first, then alphabetical — deterministic.
        entries.sort_by(|(a, _), (b, _)| {
            self.is_entry_point(b)
                .cmp(&self.is_entry_point(a))
                .then_with(|| a.cmp(b))
        });
        let mut out = String::from(
            "[WORKSPACE API SURFACE — authoritative public symbols defined in this \
             workspace. Use these EXACT names; do not invent APIs. read_file a path \
             for signatures.]\n",
        );
        for (path, syms) in entries {
            if out.len() >= self.max_block_chars {
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
}

#[async_trait]
impl MemoryProvider for ApiSurfaceProvider {
    fn name(&self) -> &str {
        "api_surface"
    }

    async fn initialize(&mut self, ctx: &SessionContext) -> anyhow::Result<()> {
        // `gather_code_files` is the only fs touch; rendering is pure (tested).
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
        self.block = self.render(&files);
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

    fn provider(packs: Vec<LanguagePack>) -> ApiSurfaceProvider {
        ApiSurfaceProvider::new(packs, &ApiSurfaceConfig::default())
    }

    #[test]
    fn builtins_cover_the_first_class_languages() {
        let names: Vec<String> = builtin_packs().into_iter().map(|p| p.name).collect();
        for lang in ["rust", "python", "bash", "c_cpp", "go", "java"] {
            assert!(
                names.contains(&lang.to_string()),
                "missing built-in: {lang}"
            );
        }
    }

    #[test]
    fn rust_pack_extracts_public_symbols_only() {
        let p = provider(builtin_packs());
        let syms = p.public_symbols(
            "x.rs",
            "pub fn open() {}\npub struct Router;\npub(crate) fn hidden() {}\nfn private() {}",
        );
        let names: Vec<&str> = syms.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"open") && names.contains(&"Router"));
        assert!(!names.contains(&"hidden") && !names.contains(&"private"));
    }

    #[test]
    fn free_form_kinds_are_not_locked_to_rust() {
        let p = provider(builtin_packs());
        // Go: exported func + type → kinds "func"/"struct", not Rust's "fn".
        let go = p.public_symbols(
            "m.go",
            "func Serve() {}\ntype Server struct {}\nfunc unexported() {}",
        );
        assert!(go.contains(&("Serve".into(), "func".into())));
        assert!(go.contains(&("Server".into(), "struct".into())));
        assert!(
            !go.iter().any(|(n, _)| n == "unexported"),
            "lowercase = unexported"
        );
        // Java: a public method → kind "method".
        let java = p.public_symbols("M.java", "  public static int run(String a) {");
        assert!(java.iter().any(|(n, k)| n == "run" && k == "method"));
    }

    #[test]
    fn c_headers_are_entry_points_via_glob() {
        let p = provider(builtin_packs());
        assert!(p.is_entry_point("src/foo.h"));
        assert!(p.is_entry_point("lib/api.hpp"));
        assert!(
            !p.is_entry_point("src/foo.c"),
            "a .c source is not an entry point"
        );
    }

    #[test]
    fn a_custom_pack_adds_a_language_with_no_code_change() {
        // Simulate ingesting an external Ruby pack (what a drop-in file would do).
        let ruby = LanguagePack {
            name: "ruby".into(),
            extensions: vec!["rb".into()],
            entry_points: vec!["*.rb".into()],
            symbols: vec![
                rule(r"^\s*def\s+([a-z_]\w*)", "method"),
                rule(r"^\s*class\s+(\w+)", "class"),
            ],
        };
        let p = provider(merge_packs(vec![builtin_packs(), vec![ruby]]));
        let syms = p.public_symbols("app.rb", "class Widget\n  def render\n  end\nend");
        assert!(syms.contains(&("Widget".into(), "class".into())));
        assert!(syms.contains(&("render".into(), "method".into())));
    }

    #[test]
    fn a_custom_pack_overrides_a_builtin_by_name() {
        // A pack named "rust" replaces the built-in (here: also surface `fn` privates).
        let custom_rust = LanguagePack {
            name: "rust".into(),
            extensions: vec!["rs".into()],
            entry_points: vec![],
            symbols: vec![rule(r"^\s*fn\s+(\w+)", "fn")],
        };
        let merged = merge_packs(vec![builtin_packs(), vec![custom_rust]]);
        let rust = merged.iter().find(|p| p.name == "rust").unwrap();
        assert_eq!(
            rust.symbols.len(),
            1,
            "the custom pack replaced the built-in rust"
        );
    }

    #[test]
    fn render_orders_entry_points_first() {
        let p = provider(builtin_packs());
        let files = vec![
            (
                "src/router.rs".to_string(),
                "pub struct Router;".to_string(),
            ),
            ("src/lib.rs".to_string(), "pub fn boot() {}".to_string()),
        ];
        let block = p.render(&files).expect("a surface");
        // lib.rs is an entry point; router.rs is not → lib.rs is listed first.
        assert!(block.find("lib.rs").unwrap() < block.find("router.rs").unwrap());
        assert!(block.contains("Router (struct)") && block.contains("boot (fn)"));
    }

    #[test]
    fn render_is_bounded() {
        let p = provider(builtin_packs());
        let files: Vec<(String, String)> = (0..500)
            .map(|i| (format!("m{i:03}.rs"), format!("pub fn f{i}() {{}}")))
            .collect();
        let block = p.render(&files).expect("a surface");
        assert!(
            block.len() <= ApiSurfaceConfig::default().max_block_chars + 100,
            "bounded: {} chars",
            block.len()
        );
        assert!(block.contains("surface truncated"), "names the truncation");
    }

    #[test]
    fn render_is_a_noop_when_nothing_public() {
        let p = provider(builtin_packs());
        assert!(p
            .render(&[("a.rs".into(), "fn private() {}".into())])
            .is_none());
        assert!(p.render(&[("README.md".into(), "# docs".into())]).is_none());
    }

    #[test]
    fn invalid_regex_in_a_rule_is_skipped_not_fatal() {
        let bad = LanguagePack {
            name: "bad".into(),
            extensions: vec!["zz".into()],
            entry_points: vec![],
            symbols: vec![rule(r"(unclosed", "x"), rule(r"^ok\s+(\w+)", "fn")],
        };
        let p = provider(vec![bad]);
        // The good rule still works; the bad one was dropped without panicking.
        assert_eq!(
            p.public_symbols("f.zz", "ok thing"),
            vec![("thing".to_string(), "fn".to_string())]
        );
    }

    #[test]
    fn shipped_example_packs_stay_valid() {
        // include_str! reads at compile time (no runtime fs) — guards the
        // contributor template + example against rot.
        for src in [
            include_str!("../../examples/language-packs/ruby.toml"),
            include_str!("../../examples/language-packs/TEMPLATE.toml"),
        ] {
            let pack: LanguagePack = toml::from_str(src).expect("example pack must parse");
            assert!(!pack.name.is_empty());
            assert!(!pack.extensions.is_empty());
            assert!(!pack.symbols.is_empty());
            // Every rule's regex compiles (so a copy-paste starting point works).
            for r in &pack.symbols {
                assert!(
                    Regex::new(&r.pattern).is_ok(),
                    "bad regex in {}: {}",
                    pack.name,
                    r.pattern
                );
            }
        }
    }
}
