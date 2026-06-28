//! Detect lazy / elided whole-file emissions — a model returning a placeholder
//! (`<the full file content remains unchanged>`, `… rest of the file …`) or an
//! empty body **instead of** the complete file. The crew apply path must REFUSE
//! these: applying one silently overwrites real code with a placeholder, which
//! only surfaces downstream as a compile error (the second failure in the #548
//! real-issue experiment). Pure — no fs, no model. (#688)

/// High-confidence elision placeholders a model emits in place of the real file.
/// Each names the *file / code / implementation* as elided, so a complete file
/// that merely comments "the public API is unchanged" does NOT trip it.
const ELISION_PHRASES: &[&str] = &[
    "the full file content remains unchanged",
    "full file content remains unchanged",
    "rest of the file remains unchanged",
    "rest of the file is unchanged",
    "rest of the file unchanged",
    "rest of file unchanged",
    "remainder of the file unchanged",
    "rest of the code remains unchanged",
    "rest of the code is unchanged",
    "rest of the implementation unchanged",
    "rest of the implementation is unchanged",
    "unchanged portion omitted",
    "omitted for brevity",
];

/// Returns a human-readable reason if `content` is a lazy/elided emission that
/// must NOT be applied, else `None` for a complete file.
pub fn lazy_emission_reason(content: &str) -> Option<String> {
    if content.trim().is_empty() {
        return Some("empty file content".to_string());
    }
    for (i, line) in content.lines().enumerate() {
        // Strip comment / markup leaders so `// <the full … unchanged>` and
        // `<the full … unchanged>` both compare equal to the bare phrase.
        let stripped = line
            .trim()
            .trim_start_matches(|c: char| "/*#<>-! \t".contains(c))
            .trim();
        let lc = stripped.to_lowercase();
        if let Some(p) = ELISION_PHRASES.iter().find(|p| lc.contains(**p)) {
            return Some(format!("elision placeholder (line {}): {p:?}", i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_the_548_placeholder() {
        // the exact emission that destroyed leaf 1 in the #548 experiment
        assert!(lazy_emission_reason("<the full file content remains unchanged>").is_some());
    }

    #[test]
    fn flags_an_elision_inside_a_real_looking_emission() {
        let body = "fn main() {\n    // ... rest of the file unchanged ...\n}\n";
        assert!(lazy_emission_reason(body).is_some());
    }

    #[test]
    fn flags_empty_or_whitespace_content() {
        assert!(lazy_emission_reason("").is_some());
        assert!(lazy_emission_reason("   \n  \t\n").is_some());
    }

    #[test]
    fn passes_a_complete_file() {
        let body = "pub fn help_lines() -> &'static [&'static str] {\n    &[\"/dgx\"]\n}\n";
        assert!(lazy_emission_reason(body).is_none());
    }

    #[test]
    fn does_not_flag_legit_prose_mentioning_unchanged() {
        // A comment noting behavior is unchanged is a COMPLETE file — only
        // file/code-elision PLACEHOLDERS trip the gate.
        let body = "// The public API is unchanged; only the internals moved.\npub fn f() {}\n";
        assert!(lazy_emission_reason(body).is_none());
        let body2 = "let msg = \"the value remains unchanged after the call\";\n";
        assert!(lazy_emission_reason(body2).is_none());
    }
}
