//! Shared def-site scope grounding (#812, #840): derive a subtask's file
//! scope from the harness's OWN grounding — a `def_sites` grep closure the
//! caller injects — rather than trusting a model's self-reported file list
//! alone. `derive_subtask_scope`/[`ground_scope`] are the ONE implementation
//! shared by crew mode's `plan::Subtask.context` (`newt-cli/src/crew.rs`) and
//! team mode's per-subtask fence (`newt-scheduler/src/team.rs`), ported here
//! (not forked) so the two paths cannot drift out of sync.
//!
//! Pure and git-free: `def_sites(symbol)` is injected as a closure, typically
//! backed by `git grep` at the call site (see `newt-cli/src/crew.rs`'s
//! `author_plan_to_plan`), so this module only reasons about the resulting
//! `path:line:content` grep lines.

/// Words that flag an adjacent unquoted token as a likely code symbol rather
/// than plain prose (e.g. "the help_lines function", "struct ConfigLoader").
const DEF_KEYWORDS: &[&str] = &[
    "function", "fn", "struct", "trait", "enum", "method", "type", "module", "mod", "macro",
];

/// A code-like identifier: snake_case (`_`), CamelCase (mixed case), or contains a
/// digit — so plain prose ("the", "parser") isn't mistaken for a symbol. (#696)
fn looks_like_identifier(t: &str) -> bool {
    let t = t.trim_matches('`');
    !t.is_empty()
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && t.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && (t.contains('_')
            || t.contains(|c: char| c.is_ascii_digit())
            || (t.chars().any(|c| c.is_ascii_uppercase())
                && t.chars().any(|c| c.is_ascii_lowercase())))
}

/// High-signal code-ish terms in `text` to grep for: slash-commands (`/dgx`),
/// backtick-quoted single tokens (`help_lines`), and code-like identifiers
/// adjacent to a def keyword ("the help_lines function", "struct Foo").
/// Distinctive enough to locate real code without the noise a bare word like
/// "help" would drown it in. (#696)
pub fn grep_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    // Backtick-quoted single tokens.
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('`') else { break };
        let span = rest[..close].trim();
        if (3..=40).contains(&span.chars().count())
            && span.split_whitespace().count() == 1
            && span.chars().any(|c| c.is_ascii_alphanumeric())
        {
            terms.push(span.to_string());
        }
        rest = &rest[close + 1..];
    }
    // Slash-commands: `/word`.
    for tok in text.split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'".contains(c)) {
        if let Some(after) = tok.strip_prefix('/') {
            let cmd: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if cmd.len() >= 2 {
                terms.push(format!("/{cmd}"));
            }
        }
    }
    // Code-like identifiers adjacent to a def keyword (#696): "the help_lines
    // function", "struct Foo", "refactor parse_config" — so the claim-check sees
    // an unquoted symbol the model named. Guarded to code-like tokens so plain
    // prose isn't grepped.
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'`".contains(c))
        .collect();
    for (i, w) in words.iter().enumerate() {
        if DEF_KEYWORDS.contains(&w.to_ascii_lowercase().as_str()) {
            let prev = i.checked_sub(1).and_then(|j| words.get(j));
            let next = words.get(i + 1);
            for cand in [prev, next].into_iter().flatten() {
                if looks_like_identifier(cand) {
                    terms.push((*cand).to_string());
                }
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms.truncate(12);
    terms
}

/// Escape ERE metacharacters so a task term matches literally inside the
/// definition pattern (grep terms are usually bare symbols, but be safe).
pub fn ere_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A POSIX-ERE pattern (for `git grep -E`) matching a Rust DEFINITION of `term`
/// — `fn`/`struct`/`trait`/`enum`/`type`/`const`/`static`/`union`/`mod`, with an
/// optional `pub`/`async`/`unsafe`, at line start, the symbol word-bounded — and
/// NOT a bare mention.
pub fn definition_grep_pattern(term: &str) -> String {
    format!(
        "^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?\
         (fn|struct|trait|enum|type|const|static|union|mod)[[:space:]]+{}([^A-Za-z0-9_]|$)",
        ere_escape(term)
    )
}

/// A term whose definitions are spread over more than this many files carries
/// no aiming signal (`main` defines in dozens) — it is skipped rather than
/// allowed to flood/evict the real target under the scope cap. (#812)
pub const AMBIGUOUS_DEF_FILES: usize = 3;

/// Cap so a derived+declared scope stays a lane, not the whole repo. (#812)
pub const SCOPE_CAP: usize = 6;

/// Derive one subtask's file scope from the harness's OWN grounding —
/// `grep_terms(instruction)` → def-site grep → the defining files. PURE over
/// the injected `def_sites` (`path:line:content` grep lines), so it is
/// unit-testable with no fs. Order-preserving dedup; ambiguous terms (defs in
/// more than [`AMBIGUOUS_DEF_FILES`] files) are skipped entirely — no signal,
/// not weak signal; git-quoted (`"`-prefixed, core.quotePath) and empty
/// paths are dropped as unusable fence entries. (#812)
pub fn derive_subtask_scope(
    instruction: &str,
    def_sites: &impl Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for term in grep_terms(instruction) {
        let mut term_files: Vec<String> = Vec::new();
        for hit in def_sites(&term) {
            if let Some(p) = hit.split(':').next() {
                if !p.is_empty() && !p.starts_with('"') && !term_files.iter().any(|q| q == p) {
                    term_files.push(p.to_string());
                }
            }
        }
        if term_files.is_empty() || term_files.len() > AMBIGUOUS_DEF_FILES {
            continue;
        }
        for p in term_files {
            if !paths.iter().any(|q| q == &p) {
                paths.push(p);
            }
        }
    }
    paths
}

/// Ground one subtask's scope: the def-site derivation LEADS (an under- or
/// mis-declaring model cannot mis-aim the fence away from the real seam), and
/// `declared` (the model's self-reported files) is APPENDED — augmentation,
/// never replacement — so a companion edit target grounding cannot see (a new
/// test file, docs, a brand-new module) stays in the lane. Capped so the lane
/// stays a lane; empty result fences nothing (meet-only downstream: the scope
/// can only narrow the writable set, never widen authority). Shared by crew
/// mode's `Plan.context` stamping and team mode's per-subtask fence — the ONE
/// grounding implementation both paths call. (#812, #840)
pub fn ground_scope(
    instruction: &str,
    declared: &[String],
    def_sites: &impl Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let mut scope = derive_subtask_scope(instruction, def_sites);
    for d in declared {
        let d = d.trim();
        if !d.is_empty() && !scope.iter().any(|q| q == d) {
            scope.push(d.to_string());
        }
    }
    scope.truncate(SCOPE_CAP);
    scope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_terms_extracts_slash_commands_and_backtick_tokens_only() {
        let terms =
            grep_terms("roll up `/dgx` help; refactor `help_lines` and /models — but not help");
        assert!(terms.contains(&"/dgx".to_string()), "{terms:?}");
        assert!(terms.contains(&"/models".to_string()), "{terms:?}");
        assert!(terms.contains(&"help_lines".to_string()), "{terms:?}");
        // Bare common words (like "help") are NOT extracted — too noisy to grep.
        assert!(!terms.iter().any(|t| t == "help"), "{terms:?}");
    }

    #[test]
    fn grep_terms_extracts_unquoted_identifiers_next_to_def_keywords() {
        // #696: the model often names a symbol unquoted — "the help_lines function".
        let t1 = grep_terms("Refactor the help_lines function in newt-cli/src/crew.rs");
        assert!(t1.contains(&"help_lines".to_string()), "{t1:?}");
        let t2 = grep_terms("add a new struct ConfigLoader to hold settings");
        assert!(t2.contains(&"ConfigLoader".to_string()), "{t2:?}");
        // Plain prose adjacent to a keyword is NOT a symbol (precision guard).
        let t3 = grep_terms("this function works well and the parser is fine");
        assert!(
            !t3.iter()
                .any(|x| x == "works" || x == "parser" || x == "the"),
            "{t3:?}"
        );
    }

    #[test]
    fn definition_grep_pattern_matches_defs_not_mentions() {
        let re = regex::Regex::new(&definition_grep_pattern("help_lines")).unwrap();
        assert!(re.is_match("fn help_lines() -> &'static [&'static str] {"));
        assert!(re.is_match("    pub async fn help_lines() {"));
        assert!(re.is_match("pub fn help_lines<T>(x: T) {"));
        assert!(!re.is_match("    // help_lines is the seam"));
        assert!(!re.is_match("    let v = self.help_lines();"));
        assert!(!re.is_match("fn help_lines_other() {")); // word-bounded, not a prefix
    }

    #[test]
    fn derive_subtask_scope_extracts_def_files_and_skips_ambiguous_terms() {
        // #812: def_sites returns `path:line:content` grep lines; the scope is
        // the DEFINING files, order-preserving, deduped. A term defined in
        // more than AMBIGUOUS_DEF_FILES files carries no aiming signal (e.g.
        // `main`) and is skipped so it cannot evict the real target; a
        // git-quoted path (core.quotePath escaping) is dropped as unusable.
        let def_sites = |sym: &str| -> Vec<String> {
            match sym {
                "humanize_duration" => vec![
                    "src/util.rs:2:pub fn humanize_duration(...)".to_string(),
                    "src/util.rs:9:fn humanize_duration_impl(...)".to_string(),
                    "src/lib.rs:11:fn humanizes()".to_string(),
                    "\"src/gr\\303\\266\\303\\237e.rs\":1:fn humanize_x()".to_string(),
                ],
                "main" => (0..14).map(|i| format!("file{i}.rs:1:fn main()")).collect(),
                _ => Vec::new(),
            }
        };
        let scope = derive_subtask_scope(
            "fix `humanize_duration` in `main` so 90s renders as 1m 30s",
            &def_sites,
        );
        assert_eq!(
            scope,
            vec!["src/util.rs".to_string(), "src/lib.rs".to_string()],
            "defining files kept; ambiguous `main` skipped; quoted path dropped"
        );
        // No grounded symbols → empty scope (fence stays off; never invented).
        assert!(derive_subtask_scope("tidy the docs", &def_sites).is_empty());
    }

    #[test]
    fn ground_scope_unions_def_sites_with_declared_and_caps() {
        // #812/#840 (§4b: augmentation, not replacement): the derived def-site
        // LEADS the scope, and the declared files are APPENDED — a companion
        // target grounding cannot see (a new test file) must stay in the
        // lane, and a mis-declaring model still cannot aim the fence away
        // from the real seam. Where derivation finds nothing, the
        // declaration alone survives.
        let def_sites = |sym: &str| -> Vec<String> {
            if sym == "humanize_duration" {
                vec!["src/util.rs:2:pub fn humanize_duration".to_string()]
            } else {
                Vec::new()
            }
        };
        let scope = ground_scope(
            "Fix `humanize_duration` in the util module",
            &["tests/util_test.rs".to_string(), "src/util.rs".to_string()],
            &def_sites,
        );
        assert_eq!(
            scope,
            vec!["src/util.rs".to_string(), "tests/util_test.rs".to_string()],
            "derived seam leads; declared companion appended; dup deduped"
        );
        let scope2 = ground_scope(
            "Create the brand-new reporting module",
            &["src/report.rs".to_string()],
            &def_sites,
        );
        assert_eq!(
            scope2,
            vec!["src/report.rs".to_string()],
            "no def-site found → the declared file survives alone"
        );
    }
}
