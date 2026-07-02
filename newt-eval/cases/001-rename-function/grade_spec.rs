// PROVENANCE: authored by the grade-spec-author workflow. strategy=hybrid.
// Survived 3 red-team rounds (8 valid games defeated). Certified:
// honest-solution PASS, unmodified-seed FAIL, corpus replay all-FAIL.
//! Canonical hidden spec for 001-rename-function — the ungameable grade (see
//! T2's spec for the house style). Dropped into the produced tree by the
//! grader as `tests/grade_spec.rs` and run via `cargo test --test grade_spec`;
//! the agent under evaluation never sees this file.
//!
//! The prompt asks for a straight, behavior-preserving rename of `greet` to
//! `hello` (definition + the in-file test's call site). This spec asserts:
//!
//!   1. Behavior is unchanged for `hello`. Two independent layers, not one:
//!      (1a) a fixed set of distinct, human-legible probes (empty string,
//!      unicode, embedded punctuation, a long string, ...); and (1b) a
//!      *runtime-randomized* batch of 256 further probes whose exact
//!      content is drawn from OS randomness (`RandomState`) + wall-clock
//!      time + a stack-address ASLR sample at the moment the grade runs —
//!      not hard-coded anywhere in this file, and therefore not
//!      knowable in advance by reading this spec's source. (1b) exists
//!      because (1a) alone was defeated in review: a gamed `hello` matched
//!      exactly the 10 literal strings this file used to hard-code and
//!      fell through to a *different* format string
//!      (`format!("Hi, {name}!")`) for everything else — every check
//!      passed because every probe this file supplied happened to be one
//!      of the 10 the gamed code special-cased. No finite fixed probe set
//!      closes that channel; only probes the gamed code cannot have seen
//!      in advance do.
//!   2. The rename is genuine, not a hedge, checked TWO ways:
//!      (2a) exactly one top-level `pub fn hello(name: &str) -> String`
//!      exists, its only bare call is `format!`, and (2b, the stronger,
//!      newer check) — recovered from the RAW, unmasked source at the
//!      exact byte range the structural scan located — `hello`'s entire
//!      body is *nothing but* `format!("Hello, {name}!")` (or the
//!      equivalent positional/`return`-wrapped forms), byte-for-byte. (2b)
//!      exists because (2a)'s "body calls nothing but format!" check was
//!      also defeated in review: `call_names()` only flags bare
//!      identifier/macro calls, so a body wrapped in
//!      `if name == "<unseen-in-any-probe>" { return String::new(); }`
//!      around the real `format!` call sailed through — the comparison
//!      and the early return use no bare calls at all, only operators and
//!      keywords, and the landmine input was never one of the (finite,
//!      fixed) strings any probe test supplied. (2b) parses the RAW body
//!      text directly: after optionally unwrapping a single
//!      `return ... ;`, what remains must be exactly one call to
//!      `format!` with the pinned literal argument(s), and *nothing else*
//!      — no room for a preceding `if`/`match`/comparison/early-return,
//!      because any such thing would leave non-empty text this parser
//!      does not recognize. This closes the landmine and the overfit
//!      hedge by the same mechanism: a body that is allowed to be
//!      *nothing but* the pinned call cannot branch on its input at all,
//!      whether the branch is keyed to 1 landmine value or 10 memorized
//!      probes.
//!      Separately (2c, unchanged from the prior revision): the bare
//!      identifier `greet` does not appear ANYWHERE ELSE in src/lib.rs
//!      once comments and string/char literals are stripped — not as a
//!      second `fn`, not as a private/renamed-away `fn` alias, not as a
//!      `const`/`static` function-pointer or closure binding kept alive
//!      under the old name, not as a `use ... as greet` re-export/import
//!      alias, and not as a leftover call site.
//!   3. The in-file `#[cfg(test)] mod tests` still exists and still
//!      contains a real `#[test]` function whose own `assert!`/
//!      `assert_eq!`/`assert_ne!` invocation calls `hello(...)` as one of
//!      ITS OWN arguments (not just "the body calls hello *and separately*
//!      the body contains an assert", which a decoupled
//!      `hello("a"); assert!(true);` vacuous pass would satisfy).
//!      *Additionally* (3b, new in this revision): `mod tests` must not
//!      contain `let`, `const`, `static`, or `|` (a closure) ANYWHERE —
//!      this trivial, dependency-free rename has no legitimate use for
//!      any of them. This closes a lexical-shadowing decoy found in
//!      review: a local binding textually named `hello`
//!      (`let hello = |_: &str| "Hello, a!".to_string();`, inserted right
//!      before the assertion) — or a module-level
//!      `const hello: fn(&str) -> String = ...;` — shadows the
//!      `use super::*;`-glob-imported crate function for the rest of that
//!      scope under ordinary Rust name resolution. The token `hello(` in
//!      the assertion is then textually present and structurally "calls
//!      hello" to any scanner that only reads tokens, but it resolves to
//!      the hardcoded local decoy, not `rename_function::hello` — proven
//!      in review by deliberately breaking the real `hello` and observing
//!      the in-file test still reported 1 passed. No scanner over
//!      stripped source text can see through Rust's scoping rules; the
//!      only sound fix is to deny the shadowing mechanism outright.
//!   4. The produced crate is honestly runnable, not just this file: this
//!      grade file compiling and linking at all already forces `cargo
//!      build` to succeed (it does `use rename_function::hello;`, which
//!      fails to compile against the unmodified seed). On top of that,
//!      `workspace_tests_still_pass` shells out to `cargo test --lib --
//!      --include-ignored` in the produced tree, because this repo's
//!      harness runs *only* `cargo test --test grade_spec`, which links
//!      `src/lib.rs` as a plain (non `--cfg test`) rlib and so never
//!      compiles or runs the crate's own `#[cfg(test)] mod tests` on its
//!      own. It parses the `cargo test` summary line itself and requires
//!      `0` failed and `>= 1` passed — not merely a zero exit code, which
//!      an `#[ignore]`d-and-never-run test would also produce.
//!
//! The structural checks are textual (`include_str!` + a noise-stripping
//! tokenizer, plus — new in this revision — a *length-preserving* masking
//! pass (`mask_noise`) used specifically to recover exact, real,
//! byte-accurate source text — real string-literal content included — at a
//! span located by scanning the (structurally sound but content-blind)
//! masked text). Their soundness rests on the invariant that the compiled
//! code is exactly the text this file scans; that invariant is enforced
//! separately by `no_textual_evasion_channels`: no macros, no
//! `#[cfg(...)]` other than the seed's own `#[cfg(test)]`, no `#[ignore]`,
//! no `include!`/`#[path]`/`env!`, no `unsafe`, no outline `mod x;`, and a
//! Cargo.toml that can't redirect the lib path, add dependencies, or add a
//! build script. Comments and string/char literals are stripped before
//! every word/keyword-level structural check, so a commented-out `greet`
//! or a `"greet"` inside an unrelated string can neither satisfy nor trip
//! them, and a bare substring search can't false-positive on the seed's
//! own `greets` test name (identifier-boundary matching only).

use rename_function::hello;

const LIB_SRC: &str = include_str!("../src/lib.rs");
const MANIFEST: &str = include_str!("../Cargo.toml");

// ---------------------------------------------------------------------------
// 1a. Behavior unchanged, across several distinct FIXED inputs
// ---------------------------------------------------------------------------

#[test]
fn behavior_preserved_for_various_names() {
    let cases: Vec<String> = vec![
        "a".to_string(),
        String::new(),
        "World".to_string(),
        "Alice Smith".to_string(),
        "O'Brien".to_string(),
        "  spaced  ".to_string(),
        "123".to_string(),
        "already, has a comma!".to_string(),
        "日本語".to_string(),
        "x".repeat(200),
    ];
    for name in &cases {
        let want = format!("Hello, {name}!");
        assert_eq!(
            hello(name),
            want,
            "hello({name:?}) must equal format!(\"Hello, {{name}}!\") — a rename must \
             preserve behavior exactly, not subtly reformat the greeting"
        );
    }
}

// ---------------------------------------------------------------------------
// 1b. Behavior unchanged across RANDOMIZED inputs unknowable in advance
// ---------------------------------------------------------------------------

/// `RandomState::new()` draws its hasher keys from OS randomness
/// (`std::collections::hash_map::hashmap_random_keys`, ultimately a
/// `getrandom`-style syscall) fresh on every construction — no external
/// crate needed, and nothing derivable from this file's own source text.
/// Mixed with the wall-clock time and a stack-address ASLR sample for extra
/// entropy, this seeds a splitmix64 stream that is different on every test
/// run and cannot have been anticipated by code written before the run
/// started.
fn runtime_entropy_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut h1 = RandomState::new().build_hasher();
    let mut h2 = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h1.write_u128(nanos);
    let stack_marker = 0u8;
    h2.write_usize(&stack_marker as *const u8 as usize);
    h1.finish() ^ h2.finish() ^ 0x9E3779B97F4A7C15
}

/// splitmix64: a small, fast, well-distributed PRNG step. Not
/// cryptographic — doesn't need to be, only unpredictable at spec-authoring
/// time, which the seed above already guarantees.
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

#[test]
fn behavior_preserved_for_unpredictable_random_inputs() {
    let mut seed = runtime_entropy_seed();
    let mut mismatches: Vec<(String, String, String)> = Vec::new();

    for _ in 0..256 {
        let len = (next_u64(&mut seed) % 48) as usize;
        let name: String = (0..len)
            .map(|_| {
                // Printable ASCII 0x20..=0x7E: always a single valid Unicode
                // scalar value, so `as char` can never panic or produce
                // invalid UTF-8 here.
                let v = (next_u64(&mut seed) % 95) as u8 + 0x20;
                v as char
            })
            .collect();
        let want = format!("Hello, {name}!");
        let got = hello(&name);
        if got != want {
            mismatches.push((name, got, want));
        }
    }

    assert!(
        mismatches.is_empty(),
        "hello() diverged from format!(\"Hello, {{name}}!\") on {} \
         randomly generated input(s) whose exact content is drawn from OS \
         randomness at test-run time and cannot be known in advance from \
         reading this spec's source. A genuine behavior-preserving rename \
         cannot special-case a fixed/sampled set of inputs and diverge on \
         everything else. First few mismatches (name, got, want): {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(5)]
    );
}

// ---------------------------------------------------------------------------
// 2. Structural checks (on comment- and literal-stripped src/lib.rs)
// ---------------------------------------------------------------------------

/// Strip `//` line comments, (nested) `/* */` block comments, `"…"` string
/// literals (incl. `r"…"` / `r#"…"#` raw strings), and `'x'` char literals
/// (lifetimes are preserved), COLLAPSING each stripped span to a single
/// space. Fine for every word/keyword-presence check below, which only
/// needs "is this identifier present anywhere" — but the collapse means
/// byte offsets in the output do NOT correspond to byte offsets in
/// `LIB_SRC`. Where exact source text (e.g. a `format!` call's actual
/// string-literal content) needs to be recovered, use `mask_noise` below
/// instead.
fn strip_noise(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' '); // preserve token boundaries
        } else if b[i] == '"' {
            // Cooked string literal: skip to the closing quote, honoring \.
            i += 1;
            while i < b.len() {
                if b[i] == '\\' {
                    i += 2;
                } else if b[i] == '"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
        } else if b[i] == 'r'
            && (i == 0 || !is_ident(b[i - 1]))
            && i + 1 < b.len()
            && (b[i + 1] == '"' || b[i + 1] == '#')
        {
            // Raw string literal r"…" / r#"…"# / r##"…"## …
            let mut hashes = 0usize;
            let mut j = i + 1;
            while j < b.len() && b[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == '"' {
                j += 1;
                'raw: while j < b.len() {
                    if b[j] == '"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while k < b.len() && b[k] == '#' && seen < hashes {
                            seen += 1;
                            k += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break 'raw;
                        }
                    }
                    j += 1;
                }
                i = j;
                out.push(' ');
            } else {
                out.push(b[i]);
                i += 1;
            }
        } else if b[i] == '\'' && i + 1 < b.len() {
            // Char literal vs lifetime: '\x' escaped, or 'x' closed 2 ahead.
            if b[i + 1] == '\\' {
                i += 2;
                while i < b.len() && b[i] != '\'' {
                    i += 1;
                }
                i += 1;
                out.push(' ');
            } else if i + 2 < b.len() && b[i + 2] == '\'' {
                i += 3;
                out.push(' ');
            } else {
                out.push(b[i]); // lifetime — keep
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Length-PRESERVING counterpart to `strip_noise`, operating byte-wise:
/// same masking rules (line/block comments, string/char literals blanked;
/// lifetimes preserved), but every masked byte is overwritten with exactly
/// one space BYTE rather than collapsed. The output is therefore the same
/// length, in bytes, as `LIB_SRC` — so any byte offset located by scanning
/// the masked text (braces, parens, keywords, none of which legitimately
/// live inside a comment or a literal) is *also* the correct byte offset
/// of that same construct in the ORIGINAL, unmasked source. That is what
/// lets callers recover exact real text — real string-literal content
/// included — for a span first located structurally, which the collapsing
/// `strip_noise` cannot do. Masking never splits a byte out of a
/// multi-byte UTF-8 sequence (spans are always bounded by single-byte
/// ASCII delimiters and fully replaced, byte for byte), so the result is
/// always valid UTF-8.
fn mask_noise(src: &str) -> String {
    fn utf8_len(byte: u8) -> usize {
        if byte < 0x80 {
            1
        } else if byte >> 5 == 0b110 {
            2
        } else if byte >> 4 == 0b1110 {
            3
        } else if byte >> 3 == 0b1_1110 {
            4
        } else {
            1
        }
    }

    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out[start..i].iter_mut().for_each(|c| *c = b' ');
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let start = i;
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out[start..i].iter_mut().for_each(|c| *c = b' ');
        } else if b[i] == b'"' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out[start..i].iter_mut().for_each(|c| *c = b' ');
        } else if b[i] == b'r'
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
            && i + 1 < b.len()
            && (b[i + 1] == b'"' || b[i + 1] == b'#')
        {
            let start = i;
            let mut hashes = 0usize;
            let mut j = i + 1;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                j += 1;
                'raw: while j < b.len() {
                    if b[j] == b'"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while k < b.len() && b[k] == b'#' && seen < hashes {
                            seen += 1;
                            k += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break 'raw;
                        }
                    }
                    j += 1;
                }
                out[start..j].iter_mut().for_each(|c| *c = b' ');
                i = j;
            } else {
                i += 1;
            }
        } else if b[i] == b'\'' && i + 1 < b.len() {
            if b[i + 1] == b'\\' {
                let start = i;
                i += 2;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                out[start..i].iter_mut().for_each(|c| *c = b' ');
            } else {
                let clen = utf8_len(b[i + 1]);
                if i + 1 + clen < b.len() && b[i + 1 + clen] == b'\'' {
                    let start = i;
                    i = i + 1 + clen + 1;
                    out[start..i].iter_mut().for_each(|c| *c = b' ');
                } else {
                    i += 1; // lifetime quote — keep as-is
                }
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out)
        .expect("mask_noise substitutes only whole ASCII-delimited spans with ASCII spaces")
}

/// Like `mask_noise`, but leaves string-literal CONTENT untouched (only `//`
/// and `/* */` comments are blanked) — used to tolerate a stray comment
/// inside `hello`'s body without corrupting the real `format!` string it
/// wraps. Best-effort on char literals (not needed for this file's bodies).
fn strip_comments_preserve_strings(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out[start..i].iter_mut().for_each(|c| *c = b' ');
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let start = i;
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out[start..i].iter_mut().for_each(|c| *c = b' ');
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out)
        .expect("strip_comments_preserve_strings substitutes only whole comment spans")
}

fn no_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `kw` appears in `s` as a standalone word (identifier boundaries).
fn contains_word(s: &str, kw: &str) -> bool {
    s.match_indices(kw).any(|(i, _)| {
        let left_ok = i == 0 || !is_ident(s[..i].chars().last().unwrap());
        let right_ok = s[i + kw.len()..]
            .chars()
            .next()
            .map_or(true, |c| !is_ident(c));
        left_ok && right_ok
    })
}

/// Inner slice of the first `{ … }` body after `start`.
fn body_after(s: &str, start: usize) -> Option<&str> {
    let open = start + s[start..].find('{')?;
    let mut depth = 0usize;
    for (off, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open + 1..open + off]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Like `body_after`, but returns the `(start, end)` byte-offset range of
/// the body instead of a slice. Meant to be called on `mask_noise`'s
/// output: because that mask is byte-length-identical to `LIB_SRC`, the
/// returned range is ALSO valid as an index into `LIB_SRC` itself, letting
/// a caller recover the real, unmasked source text of a body located
/// purely structurally.
fn body_range_after(mask: &str, start: usize) -> Option<(usize, usize)> {
    let open = start + mask[start..].find('{')?;
    let mut depth = 0usize;
    for (off, c) in mask[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open + 1, open + off));
                }
            }
            _ => {}
        }
    }
    None
}

/// ALL indices of `fn <name>` definitions (not calls, not longer idents).
/// Requiring every textual definition to pass — combined with
/// `no_textual_evasion_channels`, which guarantees every COMPILED definition
/// is textual — is what makes the textual scan sound.
fn find_fn_defs(s: &str, name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, _) in s.match_indices(name) {
        let rest = &s[i + name.len()..];
        if rest.chars().next().map_or(true, is_ident) {
            continue; // e.g. `greets` — a longer identifier, not `greet`
        }
        let before = s[..i].trim_end();
        if !before.ends_with("fn") {
            continue; // a call site, not a definition
        }
        let pre_fn = &before[..before.len() - 2];
        if pre_fn.chars().last().map_or(false, is_ident) {
            continue; // e.g. `my_fn double` — not the keyword
        }
        out.push(i);
    }
    out
}

/// ALL indices of the `fn` keyword that starts any function definition
/// (any name) — used to walk every item inside `mod tests { ... }`.
fn find_all_fn_kw_starts(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, _) in s.match_indices("fn") {
        let left_ok = i == 0 || !is_ident(s[..i].chars().last().unwrap());
        let right_ok = s[i + 2..].chars().next().map_or(true, |c| !is_ident(c));
        if left_ok && right_ok {
            out.push(i);
        }
    }
    out
}

/// Index of the `mod` keyword and the inner slice of `mod tests { ... }`'s
/// body, if such a module exists anywhere in `s`.
fn find_mod_tests(s: &str) -> Option<(usize, &str)> {
    for (i, _) in s.match_indices("mod") {
        let left_ok = i == 0 || !is_ident(s[..i].chars().last().unwrap());
        let right_ok = s[i + 3..].chars().next().map_or(true, |c| !is_ident(c));
        if !left_ok || !right_ok {
            continue;
        }
        let rest = s[i + 3..].trim_start();
        if let Some(after) = rest.strip_prefix("tests") {
            if after.chars().next().map_or(true, |c| !is_ident(c)) {
                if let Some(body) = body_after(s, i) {
                    return Some((i, body));
                }
            }
        }
    }
    None
}

/// Brace depth at byte index `i` (0 = top level of the file). Sound because
/// literals/comments were stripped, so every remaining brace is structural.
fn depth_at(s: &str, i: usize) -> i64 {
    let mut depth = 0i64;
    for c in s[..i].chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The attribute/visibility prefix of the item whose `fn`/`mod` keyword
/// starts at `kw_start`: the text since the previous item ended (`}` or
/// `;`), or the start of `s` if this is the first item.
fn item_prefix(s: &str, kw_start: usize) -> &str {
    let upto = &s[..kw_start];
    let cut = upto
        .rfind(|c| c == '}' || c == ';')
        .map(|p| p + 1)
        .unwrap_or(0);
    &upto[cut..]
}

/// Inner slice of the first `( … )` group after `start` — the paren-matching
/// twin of `body_after`, used to pull out a macro invocation's own argument
/// list (e.g. the `hello("a"), "Hello, a!"` inside `assert_eq!(...)`).
fn paren_body_after(s: &str, start: usize) -> Option<&str> {
    let open = start + s[start..].find('(')?;
    let mut depth = 0usize;
    for (off, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open + 1..open + off]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Byte index of the `)` that closes the `(` at the start of `s`, i.e. `s`
/// must begin with `(`. Used to find the exact extent of `format!(...)`
/// inside a raw, unmasked body so any trailing content after the call can
/// be detected as leftover (rather than accidentally consumed).
fn matching_paren_end(s: &str) -> Option<usize> {
    let mut depth = 0i64;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The argument-list text of every `<macro_name>!( ... )` invocation found
/// in `s` (identifier-boundary matched, so `"assert"` does not also match
/// inside `"assert_eq"`/`"assert_ne"`). Used to check that an assertion's
/// OWN arguments — not just the surrounding test body — reference the call
/// under test, closing the "call it, then assert something unrelated"
/// vacuous-pass evasion.
fn macro_call_args<'a>(s: &'a str, macro_name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (i, _) in s.match_indices(macro_name) {
        let left_ok = i == 0 || !is_ident(s[..i].chars().last().unwrap());
        if !left_ok {
            continue;
        }
        let name_end = i + macro_name.len();
        if s[name_end..].chars().next().map_or(false, is_ident) {
            continue; // e.g. matched "assert" as a prefix of "assert_eq"
        }
        let after_name = s[name_end..].trim_start();
        let Some(after_bang) = after_name.strip_prefix('!') else {
            continue; // not a macro invocation (e.g. a variable named `assert`)
        };
        if !after_bang.trim_start().starts_with('(') {
            continue; // e.g. `assert![...]` / `assert!{...}` — not used here
        }
        if let Some(args) = paren_body_after(s, name_end) {
            out.push(args);
        }
    }
    out
}

/// Every bare function/macro call name found in `body` (identifier or
/// `identifier!` immediately followed by `(`), skipping method calls
/// (`x.name(`) and path-qualified calls (`T::name(`). Because `body` is
/// already comment/string stripped, calls that only ever appeared inside a
/// string literal (e.g. `"greet(name)"` as documentation text) are already
/// gone before this ever runs.
fn call_names(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    for i in 0..chars.len() {
        if chars[i] != '(' {
            continue;
        }
        let mut end = i;
        let mut has_bang = false;
        if end > 0 && chars[end - 1] == '!' {
            has_bang = true;
            end -= 1;
        }
        let ident_end = end;
        let mut start = end;
        while start > 0 && is_ident(chars[start - 1]) {
            start -= 1;
        }
        if start == ident_end {
            continue; // no identifier immediately before `(`
        }
        let prev = if start > 0 {
            Some(chars[start - 1])
        } else {
            None
        };
        if matches!(prev, Some('.') | Some(':')) {
            continue; // method call or path-qualified call, not a bare call
        }
        let name: String = chars[start..ident_end].iter().collect();
        out.push(if has_bang { format!("{name}!") } else { name });
    }
    out
}

/// THE soundness gate for every other structural check: the compiled code
/// must be exactly the text this spec scans. Each ban below closes a channel
/// by which compiled definitions could diverge from src/lib.rs's visible
/// text. None of these constructs has any legitimate use in this one-file,
/// dependency-free rename.
#[test]
fn no_textual_evasion_channels() {
    let src = strip_noise(LIB_SRC);
    let flat = no_ws(&src);

    // --- src/lib.rs -------------------------------------------------------

    assert!(
        !contains_word(&src, "macro_rules"),
        "src/lib.rs must not define macros (`macro_rules!`): macro-emitted \
         functions hide their tokens from grading, and this rename needs no \
         macros"
    );

    // `#[cfg(...)]` other than the seed's own `#[cfg(test)]` can compile
    // text out, turning definitions into never-compiled decoys.
    for (i, _) in flat.match_indices("#[cfg") {
        assert!(
            flat[i..].starts_with("#[cfg(test)]"),
            "src/lib.rs must not use #[cfg(...)] / #[cfg_attr(...)] other \
             than the seed's own `#[cfg(test)]` on the tests module: \
             conditional compilation turns graded text into decoys"
        );
    }

    assert!(
        !src.contains("include"),
        "src/lib.rs must not use include!/include_str!/include_bytes!"
    );
    assert!(
        !flat.contains("#[path"),
        "src/lib.rs must not remap module paths with #[path = ...]"
    );
    assert!(
        !src.contains("env!"),
        "src/lib.rs must not use env!/option_env! (OUT_DIR indirection)"
    );
    assert!(
        !contains_word(&src, "unsafe"),
        "src/lib.rs must not contain `unsafe`"
    );

    // `#[ignore]` (with or without a reason string) disables a test without
    // touching its source text, so a naive "does the test still call
    // hello(...) inside a real assert" scan can't tell a live test from a
    // dead one. This one-file, dependency-free rename has no legitimate
    // reason to disable its own in-file test — ban it outright. This is a
    // textual backstop; `workspace_tests_still_pass` independently forces
    // ignored tests to run via `--include-ignored` regardless.
    assert!(
        !contains_word(&src, "ignore"),
        "src/lib.rs must not use `#[ignore]` on any test: the in-file test \
         must actually run, not be disabled while its source text is left \
         looking updated"
    );

    // Outline modules (`mod x;`) move compiled code into unscanned files.
    // Inline modules (`mod x { … }`, e.g. the seed's own `mod tests`) are
    // fine: their text is scanned.
    for (i, _) in src.match_indices("mod") {
        let left_ok = i == 0 || !is_ident(src[..i].chars().last().unwrap());
        let right = &src[i + 3..];
        if !left_ok || right.chars().next().map_or(true, is_ident) {
            continue; // part of a longer identifier
        }
        let after_ident: &str = right.trim_start().trim_start_matches(is_ident).trim_start();
        assert!(
            after_ident.starts_with('{'),
            "src/lib.rs must not declare outline modules (`mod x;`): all \
             compiled code must be inline text in src/lib.rs"
        );
    }

    // --- Cargo.toml -------------------------------------------------------
    // The spec links `rename_function` and scans src/lib.rs; the manifest
    // must not point the lib target elsewhere, pull in crates that could
    // shadow that name (dev-deps ARE linked into this integration test), or
    // run build-script codegen.
    let manifest: String = MANIFEST
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let m = no_ws(&manifest);
    assert!(
        !m.contains("dependencies"),
        "Cargo.toml must not declare any [dependencies]/[dev-dependencies]/\
         [build-dependencies]: this rename needs no crates"
    );
    assert!(
        !m.contains("build"),
        "Cargo.toml must not add a build script"
    );
    assert!(
        !m.contains("[patch") && !m.contains("[target"),
        "Cargo.toml must not add [patch]/[target] sections"
    );
    for (i, _) in m.match_indices("path=") {
        assert!(
            m[i..].starts_with("path=\"src/lib.rs\""),
            "Cargo.toml must keep every `path` pointed at src/lib.rs (no \
             lib-target redirection to an unscanned file)"
        );
    }
}

#[test]
fn hello_is_pub_top_level_and_is_the_real_formatter() {
    let src = strip_noise(LIB_SRC);

    // Exactly one `fn hello` definition, with the pinned signature, at the
    // top level of the file.
    let defs = find_fn_defs(&src, "hello");
    assert_eq!(
        defs.len(),
        1,
        "src/lib.rs must define exactly one `fn hello` (found {}): the \
         rename target must exist and be unambiguous",
        defs.len()
    );
    let i = defs[0];

    let rest = &src[i + "hello".len()..];
    let sig = no_ws(&rest[..rest.len().min(64)]);
    assert!(
        sig.starts_with("(name:&str)->String{"),
        "`fn hello` must have the exact pinned signature \
         `fn hello(name: &str) -> String`, found near: {}",
        &sig[..sig.len().min(40)]
    );
    assert_eq!(
        depth_at(&src, i),
        0,
        "`fn hello` must be a top-level function: not a method in an impl \
         block, not nested in another fn, not inside a mod"
    );

    // Must be `pub` — a bare `pub`, not `pub(crate)`/private.
    let before = src[..i].trim_end();
    let fn_kw = before.len() - 2;
    let prefix = item_prefix(&src, fn_kw);
    let prev_tok = prefix.split_whitespace().last().unwrap_or("");
    assert!(
        prev_tok == "pub",
        "`fn hello` must be `pub`, found `{prev_tok} fn hello`"
    );
    assert!(
        !prefix.contains("#[cfg"),
        "`fn hello` must not be behind a #[cfg(...)] attribute"
    );

    // Genuineness of the BODY, layer 1 (call-shape): it must call `format!`
    // and nothing else in terms of BARE calls — no delegate/wrapper call to
    // `greet` (under any name) or to any other helper. NOTE: this check
    // alone is not sufficient (see `hello_body_is_exactly_the_pinned_format_call`
    // below for the stronger, byte-exact check) — `call_names` only flags
    // bare identifier/macro calls, so e.g. `name.len()`/`name == "x"` never
    // appear here at all. It is kept as an extra layer of diagnosis, not as
    // the sole guard.
    let body = body_after(&src, i).expect("could not extract the body of `fn hello`");
    let calls = call_names(body);
    assert!(
        calls.iter().any(|c| c == "format!"),
        "`fn hello`'s body must call `format!` — a rename preserves the \
         original formatter call, it does not reimplement the greeting a \
         different way; body was: {body}"
    );
    let other: Vec<&String> = calls.iter().filter(|c| c.as_str() != "format!").collect();
    assert!(
        other.is_empty(),
        "`fn hello`'s body must call nothing but `format!` — found call(s) \
         to {other:?}. `hello` must be the real implementation, not a thin \
         wrapper that delegates to another function (including a \
         renamed-away `greet`) to do the actual work; body was: {body}"
    );
}

/// Genuineness of the body, layer 2 (byte-exact content) — the check that
/// actually closes both the "overfit to the fixed probe set" and the
/// "differential-probe landmine" evasions found in review. Both defeated
/// layer 1 above (which only bans extra *bare calls*) by wrapping the real
/// `format!` call in control flow that uses no bare calls at all
/// (comparisons, `if`, `match`, an early `return`), branching only on
/// inputs no fixed probe test happened to supply.
///
/// This check instead recovers `hello`'s REAL, unmasked body text — via
/// `mask_noise` + `body_range_after`, whose byte offsets are guaranteed
/// valid in `LIB_SRC` itself because the mask is length-preserving — and
/// parses it directly: after optionally unwrapping a single
/// `return ... ;`, what remains must be *exactly* one call to `format!`
/// with the pinned literal argument(s) (`"Hello, {name}!"` or the
/// equivalent positional `"Hello, {}!", name`), and nothing else. Any
/// preceding `if`/`match`/comparison/early-return necessarily leaves
/// leftover text this parser does not recognize, so it fails loudly rather
/// than silently accepting a body that only *happens* to behave correctly
/// on whatever inputs a test supplies.
#[test]
fn hello_body_is_exactly_the_pinned_format_call() {
    let mask = mask_noise(LIB_SRC);
    let defs = find_fn_defs(&mask, "hello");
    assert_eq!(
        defs.len(),
        1,
        "expected exactly one `fn hello` definition (found {}) — see \
         hello_is_pub_top_level_and_is_the_real_formatter for the primary \
         diagnostic",
        defs.len()
    );
    let i = defs[0];
    let (body_start, body_end) = body_range_after(&mask, i)
        .expect("could not locate the body of `fn hello` via brace matching");
    let raw_body = &LIB_SRC[body_start..body_end];
    let body_for_parsing = strip_comments_preserve_strings(raw_body);

    let mut t = body_for_parsing.trim();
    let had_return = if let Some(rest) = t.strip_prefix("return") {
        if rest.chars().next().map_or(true, |c| !is_ident(c)) {
            t = rest.trim_start();
            true
        } else {
            false
        }
    } else {
        false
    };

    let t = if had_return {
        t.strip_suffix(';')
            .unwrap_or_else(|| {
                panic!(
                    "`fn hello`'s body uses `return` but doesn't end with \
                     `;` — found: {raw_body:?}"
                )
            })
            .trim_end()
    } else {
        assert!(
            !t.ends_with(';'),
            "`fn hello`'s body must be a single tail expression with no \
             trailing `;` (or an explicit `return ...;`) — anything else \
             means the body has more than one statement in it; found: \
             {raw_body:?}"
        );
        t
    };

    let after_kw = t.strip_prefix("format!").unwrap_or_else(|| {
        panic!(
            "`fn hello`'s body must be nothing but a call to `format!` — \
             found: {raw_body:?}. A genuine rename keeps the original \
             formatter call byte-for-byte; it does not add conditionals, \
             branches, early returns, or any other logic around it (that \
             is exactly how a function could pass on a fixed/sampled set \
             of probe inputs while silently reimplementing the greeting \
             differently for everything else)."
        )
    });
    let after_kw = after_kw.trim_start();
    assert!(
        after_kw.starts_with('('),
        "`format!` must be called with parentheses; found: {raw_body:?}"
    );
    let close = matching_paren_end(after_kw)
        .unwrap_or_else(|| panic!("unbalanced parentheses in `fn hello`'s body: {raw_body:?}"));
    let inner = &after_kw[1..close];
    let remainder = after_kw[close + 1..].trim();
    assert!(
        remainder.is_empty(),
        "`fn hello`'s body must contain nothing beyond the single \
         `format!(...)` call — found trailing content {remainder:?} in \
         body: {raw_body:?}"
    );

    let inner_trimmed = inner.trim();
    let inner_no_trailing_comma = inner_trimmed
        .strip_suffix(',')
        .map(|s| s.trim_end())
        .unwrap_or(inner_trimmed);

    let matches_captured = inner_no_trailing_comma == "\"Hello, {name}!\"";
    let matches_positional = inner_no_trailing_comma
        .strip_prefix("\"Hello, {}!\"")
        .and_then(|rest| rest.trim_start().strip_prefix(','))
        .map(|rest| {
            let rest = rest.trim();
            let rest = rest.strip_suffix(',').unwrap_or(rest).trim_end();
            rest == "name"
        })
        .unwrap_or(false);

    assert!(
        matches_captured || matches_positional,
        "`fn hello`'s `format!` call must be exactly \
         `format!(\"Hello, {{name}}!\")` (or the equivalent positional \
         form `format!(\"Hello, {{}}!\", name)`) — a rename preserves the \
         original greeting text byte-for-byte, it does not reformat it, \
         special-case some inputs, or reimplement it a different way. \
         Found format! arguments: {inner_no_trailing_comma:?} (raw body: \
         {raw_body:?})"
    );
}

#[test]
fn greet_is_genuinely_gone() {
    let src = strip_noise(LIB_SRC);

    // Clearest diagnostic first: no second `fn greet` definition.
    let defs = find_fn_defs(&src, "greet");
    assert!(
        defs.is_empty(),
        "src/lib.rs must not define `fn greet` anywhere (found {} \
         definition(s)): the old name must be genuinely gone — not kept as \
         a second function, not kept as a private/renamed-away alias, and \
         not kept as the real implementation that `hello` merely delegates \
         to",
        defs.len()
    );

    // General closure: the bare identifier `greet` must not appear ANYWHERE
    // in src/lib.rs once comments/string/char literals are stripped. This
    // is strictly stronger than the `fn`-only check above and closes the
    // whole family of "the old name lives on, just not spelled `fn greet`"
    // hedges — e.g. keeping the old behavior alive under the old name as a
    // `const`/`static` function-pointer or closure binding
    // (`const greet: fn(&str) -> String = |n: &str| format!(...);`), or as
    // a `use ... as greet` re-export/import alias
    // (`pub use crate::hello as greet;`), or as a leftover call site. None
    // of those contain the token sequence `fn greet`, but all of them keep
    // `greet` fully live, in-scope, and callable — exactly what "genuinely
    // gone as a callable definition ... not kept as a private/renamed-away
    // alias" forbids. Word-boundary matching means this cannot false-
    // positive on the seed's own `greets` test function name.
    assert!(
        !contains_word(&src, "greet"),
        "the identifier `greet` must not appear anywhere in src/lib.rs \
         after a genuine rename. It was found as something other than a \
         plain `fn greet` definition — e.g. a `const`/`static` function- \
         pointer or closure binding kept under the old name, a \
         `use ... as greet` re-export/import alias, or a leftover call \
         site — any of which keeps the old name alive and callable, which \
         is not a genuine rename"
    );
}

#[test]
fn call_site_in_own_tests_updated_to_hello() {
    let src = strip_noise(LIB_SRC);

    let (mod_start, tests_body) = find_mod_tests(&src).expect(
        "src/lib.rs must still contain an in-file `mod tests { ... }` — the \
         prompt asks to UPDATE the call site, not delete the test module",
    );

    let mod_prefix = item_prefix(&src, mod_start);
    assert!(
        mod_prefix.contains("#[cfg(test)]"),
        "the in-file test module must remain gated with `#[cfg(test)]`, \
         found preceding text: {mod_prefix:?}"
    );

    let fn_starts = find_all_fn_kw_starts(tests_body);
    assert!(
        !fn_starts.is_empty(),
        "`mod tests` must still contain at least one function"
    );

    let mut found_real_test = false;
    for fn_start in &fn_starts {
        let prefix = item_prefix(tests_body, *fn_start);
        if !prefix.contains("#[test]") {
            continue; // not a test function (shouldn't happen in this seed)
        }
        let body = body_after(tests_body, *fn_start).expect("could not extract test fn body");
        let calls = call_names(body);

        assert!(
            !calls.iter().any(|c| c == "greet"),
            "an in-file test still calls the no-longer-existent `greet(...)` \
             — the call site was not updated to `hello`; body was: {body}"
        );

        // The assertion must be BUILT FROM hello's return value, not merely
        // coexist in the same test body as a discarded call to `hello`.
        // Checking "body calls hello" and "body contains an assert" as two
        // independent booleans (the previous approach) accepts
        // `hello("a"); assert!(true);` — a call whose result is thrown
        // away, next to a tautology that can never fail. That is exactly
        // the "gutted to vacuously pass" evasion the goal criterion names.
        // Instead, scope the check to each assert!/assert_eq!/assert_ne!
        // invocation's OWN argument list: at least one of them must itself
        // contain a call to `hello(...)`, which means the value being
        // asserted about actually came from `hello`.
        let mut assert_args: Vec<&str> = Vec::new();
        assert_args.extend(macro_call_args(body, "assert_eq"));
        assert_args.extend(macro_call_args(body, "assert_ne"));
        assert_args.extend(macro_call_args(body, "assert"));
        let assertion_exercises_hello = assert_args
            .iter()
            .any(|args| call_names(args).iter().any(|c| c == "hello"));

        if assertion_exercises_hello {
            found_real_test = true;
        }
    }
    assert!(
        found_real_test,
        "`mod tests` must contain a `#[test]` function with an \
         assert!/assert_eq!/assert_ne! whose OWN arguments call \
         `hello(...)` — the seed's `greets` test must be UPDATED to call \
         the renamed function as part of a real assertion, not deleted, \
         and not gutted to a vacuous pass (e.g. `hello(\"a\"); \
         assert!(true);`, which calls `hello` and contains an assert but \
         proves nothing about `hello`'s result)"
    );
}

/// Closes a lexical-shadowing decoy found in review: a local binding
/// textually named `hello` (e.g.
/// `let hello = |_: &str| "Hello, a!".to_string();`, inserted immediately
/// before the assertion) — or a module-level
/// `const hello: fn(&str) -> String = ...;` — SHADOWS the
/// `use super::*;`-glob-imported crate function `hello` for the rest of
/// that scope, under ordinary Rust name resolution. The `hello(` token
/// inside the assertion is then textually present, and
/// `call_site_in_own_tests_updated_to_hello` (a pure token/text scanner
/// with no model of Rust scoping) sees a call to `hello` inside a real
/// assert's own arguments and is satisfied — but at runtime it resolves to
/// the hardcoded local decoy, not `rename_function::hello`. Proved in
/// review by deliberately breaking the real `hello` and observing the
/// in-file test still reported 1 passed.
///
/// No text/token scanner can see through Rust's name resolution rules, so
/// the only sound fix is to deny the shadowing mechanism outright: this
/// trivial, dependency-free, one-line rename has no legitimate need for a
/// local binding (`let`), a const/static item, or a closure (`|`) anywhere
/// in its own test module.
#[test]
fn no_shadow_bindings_in_tests_module() {
    let src = strip_noise(LIB_SRC);
    let (_, tests_body) =
        find_mod_tests(&src).expect("src/lib.rs must still contain an in-file `mod tests { ... }`");

    for banned in ["let", "const", "static"] {
        assert!(
            !contains_word(tests_body, banned),
            "the in-file `mod tests` must not contain `{banned}` anywhere \
             — this trivial, dependency-free rename never needs a local \
             binding or a const/static item inside its own test module. \
             The only reason to introduce one here is to declare a \
             same-named local (e.g. `let hello = |_: &str| \"Hello, \
             a!\".to_string();`) or module-level item (e.g. \
             `const hello: fn(&str) -> String = ...;`) that SHADOWS the \
             crate's real, glob-imported `hello`, so that a textual \
             `hello(...)` inside an assertion resolves to a hard-coded \
             decoy instead of `rename_function::hello`. Found inside `mod \
             tests`: {tests_body}"
        );
    }
    assert!(
        !tests_body.contains('|'),
        "the in-file `mod tests` must not contain `|` (a closure): there \
         is no legitimate reason for this test module to define one, and \
         a closure bound to the name `hello` (or `greet`) is exactly the \
         mechanism a shadowing decoy needs. Found: {tests_body}"
    );
}

// ---------------------------------------------------------------------------
// 4. The produced crate is honestly runnable, not just this file
// ---------------------------------------------------------------------------

/// Closes a grading-*scope* gap: this repo's harness (`scripts/eval/
/// ratchet.sh`) grades a crew's work by running *exactly* `cargo test --test
/// grade_spec`. That command links `src/lib.rs` as a plain rlib dependency
/// of this one integration-test binary — it never passes `--cfg test`, so
/// the crate's own pre-existing `#[cfg(test)] mod tests { ... }` (the
/// seed's `greets` test) is never compiled or run by it. A crew can do a
/// fully genuine rename (real `hello`, `greet` gone, nothing else in this
/// file trips) and then quietly corrupt its own unit-test assertion — every
/// check above, scoped to the public API or to src/lib.rs's *text*, is
/// structurally blind to that, even though "the crate's own test suite
/// still passes" is a listed goal criterion and would now be genuinely
/// false.
///
/// Close it from inside the spec: shell out to `cargo test --lib` (which
/// DOES compile with `--cfg test`, so it exercises the crate's own unit
/// tests) against the very crate this file was dropped into, and require it
/// to succeed. Deliberately `--lib`, never bare `cargo test` — the latter
/// would rebuild and re-run every target *including this integration-test
/// binary itself*, recursing into `workspace_tests_still_pass` forever.
///
/// Two extra precautions close a second-order gap discovered in review: a
/// naive `output.status.success()` check treats "0 tests ran, 0 failed" as
/// success — exactly what happens if the crate's own test is tagged
/// `#[ignore]`. `cargo test --lib -- --include-ignored` forces any ignored
/// test to actually execute, and the parsed `N passed; M failed` summary is
/// asserted to be `M == 0 && N >= 1`, not just a zero exit code — so a
/// `#[ignore]`d-and-corrupted test is caught two ways: it either fails once
/// forced to run, or (if somehow still skipped) trips the "0 passed" check.
#[test]
fn workspace_tests_still_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir =
        std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| format!("{manifest_dir}/target"));
    // A dedicated, disposable sub-target-dir: this nested cargo invocation
    // must never share (and thus never contend or deadlock on) the build
    // lock the outer `cargo test --test grade_spec` process holds on
    // `target_dir`.
    let nested_target = format!("{target_dir}/.grade_spec_workspace_tests_check");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = std::process::Command::new(&cargo)
        .arg("test")
        .arg("--lib")
        .arg("--quiet")
        .current_dir(manifest_dir)
        .env("CARGO_TARGET_DIR", &nested_target)
        // Force any #[ignore]d test to actually execute rather than being
        // silently skipped-but-"successful". Without this, a corrupted test
        // tagged #[ignore] reports "0 passed; 0 failed; 1 ignored" and
        // exits 0, which a bare status-code check cannot tell apart from a
        // healthy suite.
        .arg("--")
        .arg("--include-ignored")
        .output()
        .expect("failed to invoke `cargo test --lib` to verify the crate's own tests");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "the produced crate's own tests (`cargo test --lib -- \
         --include-ignored`, which runs any #[cfg(test)] mod in src/lib.rs, \
         including any tests tagged #[ignore]) do not pass — the goal \
         criterion \"the crate's own test suite still passes\" is violated \
         even though `cargo test --test grade_spec` alone would never see \
         it:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );

    // Belt-and-suspenders on top of the exit code: require the summary line
    // itself to show at least one test genuinely passed and none failed.
    // This is what catches "0 passed; 0 failed" successes outright, in case
    // any other silent-skip mechanism (present or future) manages to make
    // cargo exit 0 without actually running the test.
    let summary = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("test result:"))
        .unwrap_or_else(|| {
            panic!(
                "could not find a `test result:` summary line in `cargo \
                 test --lib -- --include-ignored` output — cannot confirm \
                 any tests actually ran:\n--- stdout ---\n{stdout}\n\
                 --- stderr ---\n{stderr}"
            )
        });

    fn extract_count(summary_line: &str, label: &str) -> Option<usize> {
        summary_line.split(';').find_map(|segment| {
            let segment = segment.trim();
            if segment.ends_with(label) {
                segment
                    .trim_end_matches(label)
                    .trim()
                    .split_whitespace()
                    .last()?
                    .parse::<usize>()
                    .ok()
            } else {
                None
            }
        })
    }

    let passed = extract_count(summary, "passed").unwrap_or_else(|| {
        panic!("could not parse a `passed` count out of summary line: {summary:?}")
    });
    let failed = extract_count(summary, "failed").unwrap_or_else(|| {
        panic!("could not parse a `failed` count out of summary line: {summary:?}")
    });

    assert_eq!(
        failed, 0,
        "the produced crate's own tests report {failed} failure(s): {summary:?}"
    );
    assert!(
        passed >= 1,
        "the produced crate's own test run reports 0 passed tests \
         ({summary:?}) — the in-file test module must contain at least one \
         test that actually executes and passes, not zero executed tests \
         disguised as a clean exit"
    );
}
