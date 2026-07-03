//! Provenance: authored by the grade-spec-author workflow; strategy=hybrid;
//! survived 3 red-team rounds (9 valid games defeated); certified:
//! honest-solution PASS, unmodified-seed FAIL, corpus replay all-FAIL.
//!
//! Canonical behavioral + structural spec for 005-extract-constant — the
//! ungameable grade (see T2's spec for the pattern this follows).
//!
//! Dropped into the produced tree's `tests/` directory by `ratchet.sh` and
//! run via `cargo test --test grade_spec`; the agent under eval never sees
//! this file. It is deliberately NOT satisfied by the naive
//! `expected_patterns` regex check in `case.toml` (which only greps the
//! *diff text* for the substrings `"const SECONDS_PER_DAY"` and
//! `"SECONDS_PER_DAY \* days"` and has no model of scope, placement,
//! duplication, comments, or actually-compiled behavior).
//!
//! This revision closes FIVE real gaming techniques found against earlier
//! versions of this spec (all reproduced independently before each
//! rewrite):
//!
//!   * A **confusable-identifier substring exploit**: an earlier check
//!     used plain `str::contains`/`str::replace` to decide whether the
//!     body referenced `SECONDS_PER_DAY`, so a body that actually
//!     multiplied by a *different*, longer identifier merely containing
//!     "SECONDS_PER_DAY" as a substring (e.g. `REAL_SECONDS_PER_DAY_VALUE`)
//!     slipped through while the real, correctly-named const sat
//!     completely unused. Closed by requiring `seconds_in`'s body to
//!     structurally, exactly (modulo whitespace/`return`/redundant parens)
//!     equal `SECONDS_PER_DAY * days` or `days * SECONDS_PER_DAY` —
//!     nothing else can satisfy that.
//!
//!   * A **comment-embedded brace-depth spoof paired with a `static`
//!     swap**: an earlier byte-level brace counter didn't skip
//!     comments/strings, so a lone `}` hidden inside a `//` comment could
//!     cancel out a real `{`, making a function-local (and therefore dead)
//!     `const SECONDS_PER_DAY` register as "module scope" while the code
//!     actually used an unrelated top-level `static SECONDS_PER_DAY` of a
//!     different item kind. Closed by stripping comments and string
//!     contents (blanked to spaces, same byte offsets preserved) before
//!     any brace-depth or identifier analysis runs, and by explicitly
//!     rejecting `static`/`let`/`mut` bindings of the identifier.
//!
//!   * A **delegate-function decoy**: an earlier magic-number check only
//!     inspected `seconds_in`'s own brace block, so a body that touched
//!     the const decoratively (`let _ = SECONDS_PER_DAY;`) and then called
//!     a sibling helper containing its own untouched `60 * 60 * 24 * days`
//!     passed clean. Closed by the same exact-body structural check above:
//!     a body containing a `let _ = ...;` plus a function call can never
//!     normalize to the two accepted forms.
//!
//!   * A **header-hijack via a same-prefix decoy function name**
//!     (confirmed independently twice against a prior revision of this
//!     spec): the body-shape check located `seconds_in`'s body via a raw,
//!     non-word-boundary `str::find("fn seconds_in")`. A decoy function
//!     named `seconds_index` (or any other name literally starting with
//!     the bytes `"fn seconds_in"`, e.g. `seconds_input`) placed textually
//!     *before* the real `pub fn seconds_in` would win that raw search,
//!     so the one check that actually verifies the body's shape validated
//!     the decoy — which can be written to look perfect — while the real,
//!     exported `seconds_in` kept its original, fully unmodified
//!     `60 * 60 * 24 * days` body with no reference to the constant at
//!     all. The separate uniqueness count (`fn_occurrences`) was already
//!     word-boundary-aware and correctly reported exactly one real match,
//!     but the body-extraction helper used a *different*, unguarded
//!     search, so the two didn't agree on which function they meant.
//!     Closed by extracting the body from the *already-validated* header
//!     offset directly (see `header_occurrences` / `brace_block_from`)
//!     instead of re-searching with a raw substring find — there is now
//!     exactly one code path that decides "where is `seconds_in`", and
//!     both the uniqueness check and the body-shape check use it.
//!
//!   * A **self-test tautology, in place** (goal criterion 5): a genuine,
//!     correct extraction paired with rewriting the seed's own
//!     `assert_eq!(seconds_in(1), 86_400)` into a tautology such as
//!     `assert_eq!(seconds_in(1), seconds_in(1))` (always true, no matter
//!     what `seconds_in` computes), plus a dead `let _x: u64 = 86_400;`
//!     statement kept only to satisfy a token-presence heuristic
//!     ("does the test module contain the strings `seconds_in(1)` and
//!     `86_400` somewhere"). Proven to supply zero regression protection:
//!     against a copy with a deliberately broken `seconds_in`, the gutted
//!     test still reported `ok`. Closed by parsing `assert_eq!` calls
//!     inside `mod tests` structurally (balanced-delimiter argument
//!     splitting, not substring containment) and requiring at least one
//!     call shaped exactly like `assert_eq!(seconds_in(<int literal N>),
//!     <pure integer expression>)` whose two sides are texually distinct
//!     and whose right-hand side genuinely evaluates to `86_400 * N` —
//!     a self-comparison or any call whose right-hand side itself invokes
//!     `seconds_in` is explicitly rejected.
//!
//!   * A **self-test tautology, relocated** (three independently
//!     confirmed variants of the same root cause, found against the
//!     revision that closed the bullet above): that revision's "genuine
//!     assert_eq! exists" scan searched the *entire* `mod tests { ... }`
//!     block for a single qualifying call, rather than requiring it to
//!     live inside `one_day`'s *own* body. That let an attacker gut the
//!     seed's actual `#[test] fn one_day` — into `assert!(true)`, into a
//!     self-comparison tautology `assert_eq!(seconds_in(1),
//!     seconds_in(1))`, or into a tautology with the real assertion
//!     relocated into a second, never-invoked, non-`#[test]` decoy
//!     function — while a genuinely-shaped `assert_eq!` planted in an
//!     unrelated sibling test (or a dead sibling function) elsewhere in
//!     the same module satisfied the module-wide scan. `cargo test --lib`
//!     still printed `tests::one_day ... ok` (a no-op or a tautology both
//!     trivially "pass"), so both signals looked green while the seed's
//!     specific named regression test — the one goal criterion 5 calls
//!     out by name and by body — had zero remaining regression coverage.
//!     Proven independently for each variant: a deliberately broken
//!     `seconds_in` still made `tests::one_day` report `ok`. Closed by no
//!     longer scanning the whole module: `fn one_day` is now located as a
//!     single, word-boundary-unique, `#[test]`-annotated header *inside*
//!     the already-validated `mod tests` block (an un-annotated
//!     same-named decoy is rejected outright, independent of whether it
//!     ever runs), its body is extracted from that exact offset via the
//!     same `brace_block_from` used elsewhere, and the genuine-assertion
//!     search now runs only over *that* body — a decoy sitting anywhere
//!     else in the module, dead or alive, can no longer satisfy it.
//!
//! The spec still: statically inspects the produced `src/lib.rs` for a
//! single, correctly-scoped, correctly-valued, correctly-placed
//! `const SECONDS_PER_DAY: u64`; bans the generic textual-evasion channels
//! this repo's other cases use (`macro_rules!`, `include!`, `#[path`,
//! non-test `#[cfg(...)]`, a redirected `Cargo.toml` `[lib] path`);
//! exercises the compiled public API across several `days` values, not
//! just the seed's n=1 probe; and actually runs `cargo test --lib` inside
//! the produced crate (in its own nested target dir) to confirm the seed's
//! own `tests::one_day` still exists and passes, since
//! `cargo test --test grade_spec` alone never compiles `#[cfg(test)]` code
//! and would otherwise never notice that assertion being gutted — and now
//! additionally confirms structurally that it wasn't gutted into an
//! always-true decoy that merely happens to still say "ok".

use extract_constant::seconds_in;
use std::process::Command;

fn lib_src() -> String {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).to_string()
}

fn cargo_toml_src() -> String {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).to_string()
}

/// Returns a same-length copy of `src` with the *contents* of `//` line
/// comments, `/* ... */` block comments, and `"..."` string / `'x'` char
/// literals blanked out to spaces (newlines preserved). Every other byte
/// is untouched, so byte offsets computed against the result line up
/// exactly with offsets into the original `src`.
///
/// This exists so that a lone brace character sitting inside a comment
/// (e.g. `// legacy marker: }`) can never desynchronize a byte-level
/// brace-depth scan — the exploit this closes was verified to work
/// against a version of this spec that scanned raw source directly.
fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = bytes.to_vec();
    let mut i = 0usize;
    while i < n {
        if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let mut j = i;
            while j < n && bytes[j] != b'\n' {
                out[j] = b' ';
                j += 1;
            }
            i = j;
            continue;
        }
        if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            out[i] = b' ';
            out[i + 1] = b' ';
            let mut j = i + 2;
            while j + 1 < n && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                out[j] = b' ';
                j += 1;
            }
            if j + 1 < n {
                out[j] = b' ';
                out[j + 1] = b' ';
                j += 2;
            } else {
                // unterminated block comment; blank to EOF defensively
                while j < n {
                    out[j] = b' ';
                    j += 1;
                }
            }
            i = j;
            continue;
        }
        if bytes[i] == b'"' {
            out[i] = b' ';
            let mut j = i + 1;
            while j < n && bytes[j] != b'"' {
                if bytes[j] == b'\\' && j + 1 < n {
                    out[j] = b' ';
                    out[j + 1] = b' ';
                    j += 2;
                    continue;
                }
                out[j] = b' ';
                j += 1;
            }
            if j < n {
                out[j] = b' ';
                j += 1;
            }
            i = j;
            continue;
        }
        if bytes[i] == b'\'' {
            // Only treat as a char literal (not a lifetime like 'a) if a
            // closing quote appears within the next few bytes, covering
            // 'a', '\n', '\'', '\\', 'A' worst case is rare here.
            let mut j = i + 1;
            let mut closed = None;
            while j < n && j <= i + 4 {
                if bytes[j] == b'\'' {
                    closed = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = closed {
                for k in i..=end {
                    out[k] = b' ';
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    String::from_utf8(out).expect("blanking comments/strings preserves valid UTF-8")
}

/// Brace depth at byte offset `at` in `src` (module scope == 0). `src`
/// must already have comments/strings blanked via
/// `strip_comments_and_strings` or this is spoofable (see module docs).
fn brace_depth_at(src: &str, at: usize) -> i32 {
    let mut depth = 0i32;
    for &b in src.as_bytes()[..at].iter() {
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The identifier-word immediately preceding byte offset `at` in `src`
/// (skipping trailing whitespace), e.g. "const" in "...pub const SECONDS".
fn preceding_word(src: &str, at: usize) -> Option<&str> {
    let before = src[..at].trim_end();
    let start = before
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &before[start..];
    if word.is_empty() {
        None
    } else {
        Some(word)
    }
}

/// True word-boundary occurrences of `needle` (an identifier) in `src`.
fn identifier_occurrences(src: &str, needle: &str) -> Vec<usize> {
    let bytes = src.as_bytes();
    src.match_indices(needle)
        .filter(|(i, _)| {
            let before_ok =
                *i == 0 || !(bytes[i - 1] as char).is_alphanumeric() && bytes[i - 1] != b'_';
            let end = i + needle.len();
            let after_ok =
                end == bytes.len() || !(bytes[end] as char).is_alphanumeric() && bytes[end] != b'_';
            before_ok && after_ok
        })
        .map(|(i, _)| i)
        .collect()
}

/// Byte offsets in `src` where the literal text `header` appears such
/// that the character immediately *following* the match is not an
/// identifier character. This deliberately does NOT check the character
/// *before* the match (callers like `"fn seconds_in"` and `"mod tests"`
/// already include the preceding keyword, and requiring a boundary before
/// `"fn"`/`"mod"` themselves would be redundant); what it guards against
/// is a same-prefixed decoy trailing off from the match, e.g. `"fn
/// seconds_in"` must not be treated as found inside `"fn seconds_index"`.
///
/// This is the fix for the header-hijack exploit described in the module
/// docs: an earlier version of this spec located `seconds_in`'s (and
/// `mod tests`'s) body via a raw, unguarded `str::find`, which a
/// textually-earlier, same-prefixed decoy item could hijack. Every caller
/// that needs to find "the" occurrence of a header now goes through this
/// function and asserts there is exactly one match, then extracts the
/// body from *that exact offset* — never via a second, independent raw
/// search that could disagree with the uniqueness check.
fn header_occurrences(src: &str, header: &str) -> Vec<usize> {
    let bytes = src.as_bytes();
    src.match_indices(header)
        .map(|(i, _)| i)
        .filter(|&i| {
            let end = i + header.len();
            end == bytes.len() || (!(bytes[end] as char).is_alphanumeric() && bytes[end] != b'_')
        })
        .collect()
}

/// Tiny integer-arithmetic evaluator (+, -, *, /, unary -, parens,
/// `_`-separated literals) used both to check the const's right-hand side
/// genuinely computes to 86_400 (however it's spelled: `86_400`,
/// `60 * 60 * 24`, ...) and to check a test assertion's right-hand side.
/// Returns `None` on anything it can't parse (unsupported characters,
/// unbalanced parens, trailing garbage) rather than panicking, so callers
/// can use it to test whether an arbitrary snippet of source is even a
/// pure integer expression at all.
fn try_eval_int_expr(expr: &str) -> Option<i64> {
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Tok {
        Num(i64),
        Plus,
        Minus,
        Star,
        Slash,
        LParen,
        RParen,
    }
    let mut toks = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let mut num = String::new();
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                if chars[i] != '_' {
                    num.push(chars[i]);
                }
                i += 1;
            }
            toks.push(Tok::Num(num.parse().ok()?));
        } else {
            toks.push(match c {
                '+' => Tok::Plus,
                '-' => Tok::Minus,
                '*' => Tok::Star,
                '/' => Tok::Slash,
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                _ => return None,
            });
            i += 1;
        }
    }

    fn parse_expr(toks: &[Tok], pos: &mut usize) -> Option<i64> {
        let mut val = parse_term(toks, pos)?;
        while *pos < toks.len() {
            match toks[*pos] {
                Tok::Plus => {
                    *pos += 1;
                    val += parse_term(toks, pos)?;
                }
                Tok::Minus => {
                    *pos += 1;
                    val -= parse_term(toks, pos)?;
                }
                _ => break,
            }
        }
        Some(val)
    }
    fn parse_term(toks: &[Tok], pos: &mut usize) -> Option<i64> {
        let mut val = parse_factor(toks, pos)?;
        while *pos < toks.len() {
            match toks[*pos] {
                Tok::Star => {
                    *pos += 1;
                    val *= parse_factor(toks, pos)?;
                }
                Tok::Slash => {
                    *pos += 1;
                    let d = parse_factor(toks, pos)?;
                    if d == 0 {
                        return None;
                    }
                    val /= d;
                }
                _ => break,
            }
        }
        Some(val)
    }
    fn parse_factor(toks: &[Tok], pos: &mut usize) -> Option<i64> {
        match toks.get(*pos) {
            Some(Tok::Num(n)) => {
                *pos += 1;
                Some(*n)
            }
            Some(Tok::LParen) => {
                *pos += 1;
                let v = parse_expr(toks, pos)?;
                if toks.get(*pos) != Some(&Tok::RParen) {
                    return None;
                }
                *pos += 1;
                Some(v)
            }
            Some(Tok::Minus) => {
                *pos += 1;
                Some(-parse_factor(toks, pos)?)
            }
            _ => None,
        }
    }

    let mut pos = 0;
    let v = parse_expr(&toks, &mut pos)?;
    if pos != toks.len() {
        return None;
    }
    Some(v)
}

/// Like `try_eval_int_expr`, but panics with a descriptive message on
/// failure — for call sites where the expression is expected (by
/// construction, e.g. the already-located const initializer) to be a
/// valid integer expression, and a parse failure is itself a spec
/// violation worth failing loudly on.
fn eval_int_expr(expr: &str) -> i64 {
    try_eval_int_expr(expr).unwrap_or_else(|| {
        panic!(
            "grade_spec's arithmetic evaluator could not parse {expr:?} as a pure integer \
             expression (only + - * / ( ) and _-separated integer literals are supported)"
        )
    })
}

/// Extract the `{ ... }` block whose opening brace is the first `{`
/// found at or after byte offset `from`, returning its interior text.
/// Callers must pass an offset that has already been validated (e.g. via
/// `header_occurrences` + a uniqueness check) to point at the real item —
/// this function performs no header search of its own, precisely so that
/// there is exactly one code path deciding "which function/module is
/// this", not two independent searches that could disagree (see the
/// header-hijack exploit in the module docs).
fn brace_block_from(src: &str, from: usize) -> String {
    let bytes = src.as_bytes();
    let open = src[from..]
        .find('{')
        .map(|i| from + i)
        .unwrap_or_else(|| panic!("no opening brace found at or after byte offset {from}"));
    let mut depth = 0i32;
    let mut i = open;
    loop {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return src[open + 1..i].to_string();
                }
            }
            _ => {}
        }
        i += 1;
        if i >= bytes.len() {
            panic!("unbalanced braces scanning block from byte offset {from}");
        }
    }
}

/// Strips one layer of fully-wrapping outer parens at a time, e.g.
/// `"(SECONDS_PER_DAY*days)"` -> `"SECONDS_PER_DAY*days"`. Only strips
/// when the very first `(` is the match for the very last `)` (i.e. the
/// parens genuinely wrap the whole expression, not just a sub-term).
fn strip_matching_outer_parens(mut s: String) -> String {
    loop {
        if s.len() < 2 || !s.starts_with('(') || !s.ends_with(')') {
            return s;
        }
        let bytes = s.as_bytes();
        let mut depth = 0i32;
        let mut wraps_whole = true;
        for (idx, &b) in bytes.iter().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && idx != bytes.len() - 1 {
                        wraps_whole = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if wraps_whole && depth == 0 {
            s = s[1..s.len() - 1].to_string();
        } else {
            return s;
        }
    }
}

/// Normalizes a function-body's tail expression for exact structural
/// comparison: trims a leading `return`, a trailing `;`, all whitespace,
/// and any fully-wrapping outer parens.
fn normalize_tail_expr(body: &str) -> String {
    let mut t = body.trim().to_string();
    if let Some(rest) = t.strip_prefix("return") {
        let boundary_ok = rest
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric() && c != '_')
            .unwrap_or(true);
        if boundary_ok {
            t = rest.trim_start().to_string();
        }
    }
    if t.trim_end().ends_with(';') {
        let trimmed = t.trim_end();
        t = trimmed[..trimmed.len() - 1].trim_end().to_string();
    }
    let no_ws: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    strip_matching_outer_parens(no_ws)
}

/// Splits every `assert_eq!( ... )` invocation found in `src` into its
/// comma-separated top-level arguments (2, for `assert_eq!(a, b)`, or 3
/// if a format-string message is attached), using a delimiter-depth-aware
/// scan over `(`/`[`/`{` so that a nested call like `seconds_in(1)`
/// inside an argument does not get mistaken for an argument separator.
///
/// This exists so the seed's own regression test can be checked
/// structurally rather than via substring containment — the exploit this
/// closes (see module docs) rewrote the assertion into a tautology like
/// `assert_eq!(seconds_in(1), seconds_in(1))` while keeping the literal
/// tokens `seconds_in(1)` and `86_400` present elsewhere purely to fool a
/// "does the text contain these substrings" check.
fn assert_eq_call_args(src: &str) -> Vec<Vec<String>> {
    let bytes = src.as_bytes();
    let mut calls = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = src[search_from..].find("assert_eq!") {
        let mut i = search_from + rel + "assert_eq!".len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'(' {
            search_from = search_from + rel + "assert_eq!".len();
            continue;
        }
        let open = i;
        let mut depth = 0i32;
        let mut j = open;
        let mut args = Vec::new();
        let mut arg_start = open + 1;
        loop {
            if j >= bytes.len() {
                panic!("unbalanced delimiters scanning an assert_eq!(...) call");
            }
            match bytes[j] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        args.push(src[arg_start..j].trim().to_string());
                        break;
                    }
                }
                b',' if depth == 1 => {
                    args.push(src[arg_start..j].trim().to_string());
                    arg_start = j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        calls.push(args);
        search_from = j + 1;
    }
    calls
}

#[test]
fn const_is_module_scope_before_fn_with_correct_value() {
    let raw = lib_src();
    // All structural analysis below runs against `scan`, a same-length
    // copy of `raw` with comment/string interiors blanked to spaces, so a
    // brace or identifier hidden inside a comment can never desync a
    // byte-level scan (see module docs: comment-embedded brace exploit).
    let scan = strip_comments_and_strings(&raw);

    // --- textual-evasion channels this task has no legitimate reason to use ---
    assert!(
        !scan.contains("macro_rules!"),
        "seconds_in extraction has no legitimate reason to introduce a macro; \
         macro_rules! in src/lib.rs is treated as an attempt to hide the real definition"
    );
    assert!(
        !scan.contains("include!"),
        "src/lib.rs must not source code from an unscanned file via include!()"
    );
    assert!(
        !scan.contains("#[path"),
        "src/lib.rs must not redirect module resolution via #[path = ...]"
    );
    for cfg_use in scan.match_indices("#[cfg(") {
        let start = cfg_use.0;
        let end = scan[start..]
            .find(']')
            .map(|i| start + i + 1)
            .unwrap_or(scan.len());
        let attr = &scan[start..end];
        assert_eq!(
            attr, "#[cfg(test)]",
            "the only #[cfg(...)] attribute allowed in src/lib.rs is #[cfg(test)] on the test \
             module; found {attr:?}, which could be gating a decoy definition"
        );
    }
    assert_eq!(
        cargo_toml_src().matches("src/lib.rs").count(),
        1,
        "Cargo.toml's [lib] path must still point at src/lib.rs"
    );

    // --- find seconds_in's definition site (word-boundary aware, so a
    //     same-prefixed decoy like `fn seconds_index` or `fn
    //     seconds_input` can't be mistaken for it — and so that the SAME
    //     validated offset, not a second independent search, is what
    //     gets used to extract the body below) ---
    let fn_headers = header_occurrences(&scan, "fn seconds_in");
    assert_eq!(
        fn_headers.len(),
        1,
        "expected exactly one `fn seconds_in` definition in src/lib.rs, found {} \
         (a same-prefixed decoy function, e.g. `fn seconds_index`, is not allowed even if it's \
         never called — it must not exist at all)",
        fn_headers.len()
    );
    let fn_offset = fn_headers[0];

    // --- find every word-boundary occurrence of SECONDS_PER_DAY ---
    let occurrences = identifier_occurrences(&scan, "SECONDS_PER_DAY");
    assert!(
        !occurrences.is_empty(),
        "no `SECONDS_PER_DAY` identifier found anywhere in src/lib.rs — the constant was never extracted"
    );

    // Reject it being bound via `static`/`let`/`mut` anywhere instead of
    // `const` — closes the static-swap half of the comment-brace exploit,
    // independent of the brace-depth fix below.
    let non_const_bindings: Vec<(usize, &str)> = occurrences
        .iter()
        .copied()
        .filter_map(|off| {
            preceding_word(&scan, off).and_then(|w| {
                if matches!(w, "static" | "let" | "mut") {
                    Some((off, w))
                } else {
                    None
                }
            })
        })
        .collect();
    assert!(
        non_const_bindings.is_empty(),
        "SECONDS_PER_DAY must be bound exactly once via `const`, not via {non_const_bindings:?} \
         (a `static`/`let` binding is a differently-scoped/differently-kinded item, even if \
         same-named — see goal criterion 1)"
    );

    let declaring: Vec<usize> = occurrences
        .iter()
        .copied()
        .filter(|&off| preceding_word(&scan, off) == Some("const"))
        .collect();
    assert_eq!(
        declaring.len(),
        1,
        "expected exactly one `const SECONDS_PER_DAY` declaration in src/lib.rs, found {} \
         (shadowing/duplicate const declarations are not allowed)",
        declaring.len()
    );
    let decl_offset = declaring[0];

    assert_eq!(
        brace_depth_at(&scan, decl_offset),
        0,
        "`const SECONDS_PER_DAY` must be declared at module scope (top level of the file), \
         not nested inside a function or other block"
    );
    assert!(
        decl_offset < fn_offset,
        "`const SECONDS_PER_DAY` must be declared textually before `seconds_in`'s definition \
         (found the const at byte {decl_offset}, but seconds_in at byte {fn_offset})"
    );

    // --- parse "const SECONDS_PER_DAY <TYPE> = <EXPR>;" and check both ---
    let decl_start = scan[..decl_offset]
        .rfind("const")
        .expect("preceding_word already confirmed a `const` keyword precedes this occurrence");
    let semi_rel = scan[decl_offset..]
        .find(';')
        .expect("const SECONDS_PER_DAY declaration has no terminating `;`");
    let decl_stmt = &scan[decl_start..decl_offset + semi_rel];
    let colon_rel = decl_stmt
        .find(':')
        .expect("const SECONDS_PER_DAY has no type annotation (`: u64`)");
    let eq_rel = decl_stmt
        .find('=')
        .expect("const SECONDS_PER_DAY has no `=` initializer");
    let ty = decl_stmt[colon_rel + 1..eq_rel].trim();
    assert_eq!(
        ty, "u64",
        "SECONDS_PER_DAY must be declared as `u64` (found {ty:?})"
    );
    let rhs = &decl_stmt[eq_rel + 1..];
    let value = eval_int_expr(rhs);
    assert_eq!(
        value, 86_400,
        "const SECONDS_PER_DAY's value must genuinely evaluate to 86_400 (60*60*24); \
         got {value} from expression {rhs:?}"
    );

    // --- seconds_in's body must EXACTLY compute SECONDS_PER_DAY * days ---
    // (or days * SECONDS_PER_DAY), modulo whitespace/`return`/redundant
    // parens, extracted from the SAME `fn_offset` validated above as the
    // unique real definition (never via a second, independent raw
    // search — that mismatch is exactly the header-hijack exploit the
    // module docs describe and this revision closes). This single
    // structural check — rather than a substring "contains
    // SECONDS_PER_DAY and no leftover digits" heuristic — also closes two
    // other distinct gaming techniques at once: a confusable longer
    // identifier merely containing "SECONDS_PER_DAY" as a substring, and
    // a discarded `let _ = SECONDS_PER_DAY;` reference beside real work
    // done elsewhere or in a delegate/helper function.
    let body = brace_block_from(&scan, fn_offset);
    let normalized = normalize_tail_expr(&body);
    let accepted = ["SECONDS_PER_DAY*days", "days*SECONDS_PER_DAY"];
    assert!(
        accepted.contains(&normalized.as_str()),
        "seconds_in's body must compute its result by directly multiplying SECONDS_PER_DAY and \
         days (e.g. `SECONDS_PER_DAY * days`) — not via a discarded reference, a parallel/\
         duplicated inline computation, or a delegate/helper function that does the real work \
         out of view. Got body {body:?}, which normalizes to {normalized:?} \
         (expected one of {accepted:?})"
    );
}

#[test]
fn seconds_in_matches_86400_times_n_for_many_n() {
    for &n in &[0u64, 1, 2, 3, 365, 10_000, 1_000_000_000] {
        let expected = 86_400u64 * n;
        assert_eq!(
            seconds_in(n),
            expected,
            "seconds_in({n}) should be 86_400 * {n} = {expected}"
        );
    }
}

#[test]
fn crate_own_test_module_still_exists_and_passes() {
    // `cargo test --test grade_spec` (this binary) links src/lib.rs as a
    // plain rlib and never compiles `#[cfg(test)]` code, so the seed's own
    // `mod tests { fn one_day() ... }` would silently never run if we
    // didn't drive it explicitly here. Run it for real, in a nested target
    // dir so it can't deadlock on the outer build's target-dir lock.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let outer_target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let nested_target = format!("{outer_target}/grade_spec_selftest");

    let output = Command::new(env!("CARGO"))
        .args(["test", "--lib"])
        .env("CARGO_TARGET_DIR", &nested_target)
        .current_dir(manifest_dir)
        .output()
        .expect("failed to spawn `cargo test --lib` to check the crate's own test suite");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the crate's own `cargo test --lib` failed (its pre-existing self-test must still pass):\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("tests::one_day ... ok"),
        "the crate's own `tests::one_day` test did not run (or was renamed/deleted); \
         full output:\n{stdout}"
    );

    // Now confirm STRUCTURALLY (not via substring containment, and not via
    // a whole-module scan) that `one_day` itself — not some other function
    // sharing the module — actually still checks something real.
    //
    // A prior version of this spec only checked that the
    // (comment/string-stripped) test module contained the tokens
    // "seconds_in(1)" and "86_400" *somewhere*, which a tautology plus a
    // throwaway `let _x: u64 = 86_400;` satisfied. The *next* revision
    // fixed that by structurally parsing `assert_eq!` calls, but still
    // searched the whole `mod tests` block for any one qualifying call —
    // which a genuinely-shaped `assert_eq!` planted in an unrelated
    // sibling function (a second `#[test]`, or a dead never-invoked
    // function) could satisfy while `one_day`'s own body was gutted into
    // a no-op or a self-comparison tautology. Both variants were proven to
    // supply zero regression protection: against a copy with a
    // deliberately broken `seconds_in`, the gutted `one_day` still
    // reported `ok`. Closed by locating `fn one_day` itself as a single,
    // word-boundary-unique, directly-`#[test]`-annotated header inside the
    // already-validated `mod tests` block, extracting *its own* body from
    // that exact offset, and requiring the genuine-assertion search to
    // pass over *that* body alone — nothing planted elsewhere in the
    // module can satisfy it anymore.
    let scan = strip_comments_and_strings(&lib_src());
    let mod_headers = header_occurrences(&scan, "mod tests");
    assert_eq!(
        mod_headers.len(),
        1,
        "expected exactly one `mod tests` in src/lib.rs, found {} \
         (a same-prefixed decoy module could otherwise hijack which body gets checked, the same \
         class of exploit closed for `fn seconds_in` above)",
        mod_headers.len()
    );
    let mod_open = scan[mod_headers[0]..]
        .find('{')
        .map(|i| mod_headers[0] + i)
        .expect("mod tests has no opening brace");
    let test_mod = brace_block_from(&scan, mod_headers[0]);

    let one_day_headers = header_occurrences(&test_mod, "fn one_day");
    assert_eq!(
        one_day_headers.len(),
        1,
        "expected exactly one `fn one_day` inside `mod tests`, found {} \
         (a same-prefixed or same-named decoy function elsewhere in the module — even a dead, \
         never-invoked one — is not allowed to exist; the SPECIFIC pre-existing test named \
         `one_day` is what goal criterion 5 requires to survive unaltered)",
        one_day_headers.len()
    );
    let one_day_offset = one_day_headers[0];

    // `one_day` must genuinely be a `#[test]` (immediately annotated, not
    // just coincidentally named the same as one) — otherwise a plain,
    // never-executed decoy function literally named `one_day` sitting next
    // to the real corrupted test could be used to smuggle a "real" body
    // past a check that didn't verify it actually runs. (The dynamic
    // `tests::one_day ... ok` check above already implies this in
    // practice, since only `#[test]` fns show up in that harness output,
    // but this makes the requirement explicit and gives a clearer failure
    // message if the two ever disagree.)
    let before_fn = test_mod[..one_day_offset].trim_end();
    assert!(
        before_fn.ends_with("#[test]"),
        "`fn one_day` inside `mod tests` must be directly annotated with `#[test]` (found \
         {before_fn:?} immediately before it) — an un-annotated function merely named `one_day` \
         does not count as the seed's pre-existing regression test"
    );

    // Reject a same-named decoy `mod` or nested block hiding a second
    // `fn one_day` at a different brace depth by confirming the one we
    // found is a direct member of `mod tests` (depth 1 relative to the
    // module's own opening brace), not nested inside some other item.
    let relative_depth =
        brace_depth_at(&scan, mod_open + 1 + one_day_offset) - brace_depth_at(&scan, mod_open + 1);
    assert_eq!(
        relative_depth, 0,
        "`fn one_day` must be declared directly inside `mod tests`, not nested inside another \
         block"
    );

    let one_day_body = brace_block_from(&test_mod, one_day_offset);
    let calls = assert_eq_call_args(&one_day_body);
    let genuine = calls.iter().any(|args| {
        if args.len() < 2 {
            return false;
        }
        let lhs: String = args[0].chars().filter(|c| !c.is_whitespace()).collect();
        let rhs: String = args[1].chars().filter(|c| !c.is_whitespace()).collect();

        // Reject tautologies/self-comparisons outright: identical sides,
        // or the right-hand side itself calling seconds_in (so the check
        // can never distinguish a correct implementation from a broken
        // one that merely agrees with itself).
        if lhs == rhs || rhs.contains("seconds_in(") {
            return false;
        }

        // Left-hand side must be exactly `seconds_in(<integer literal>)`.
        let Some(inner) = lhs
            .strip_prefix("seconds_in(")
            .and_then(|s| s.strip_suffix(')'))
        else {
            return false;
        };
        if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit() || c == '_') {
            return false;
        }
        let Ok(n) = inner.replace('_', "").parse::<i64>() else {
            return false;
        };

        // Right-hand side must be a pure integer expression genuinely
        // evaluating to 86_400 * n.
        try_eval_int_expr(&rhs) == Some(86_400 * n)
    });

    assert!(
        genuine,
        "`fn one_day`'s OWN body no longer contains a genuine `assert_eq!(seconds_in(N), \
         86_400 * N)`-shaped check — it appears to have been altered into a tautology (e.g. \
         comparing seconds_in(N) to itself), a self-comparison, a no-op, or otherwise decoupled \
         from checking a genuine numeric value. A similarly-shaped genuine assertion existing \
         elsewhere in `mod tests` does not count: the SPECIFIC pre-existing test named \
         `one_day` must be the one that still checks something real (goal criterion 5). Found \
         these assert_eq! call argument lists inside one_day's own body: {calls:?}"
    );
}
