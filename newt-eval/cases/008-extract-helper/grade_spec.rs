// Provenance: authored by the grade-spec-author workflow.
// Strategy: hybrid.
// Survived 3 red-team rounds (9 valid games defeated).
// Certified: honest-solution PASS, unmodified-seed FAIL, corpus replay all-FAIL.
//! Canonical hidden spec for 008-extract-helper — the ungameable grade (see
//! T2's spec for the house style). Dropped into the produced tree by the
//! grader and run via `cargo test --test grade_spec`; the agent never sees
//! it.
//!
//! It asserts THREE things:
//!   1. Behavior is unchanged AS A PROPERTY, not a finite oracle:
//!      `double_first`/`double_last` must return `Some(2*k)` / `None` on
//!      empty for an exhaustive band (-65536..=65536), 20k LCG-driven
//!      samples across the full doubling-safe i32 range, and the half-range
//!      extremes. A helper that memorizes a probe set fails on the first
//!      un-memorized input.
//!   2. Structure is genuinely refactored: exactly one top-level PRIVATE,
//!      un-`cfg`ed `fn double(n: i32) -> i32` exists in src/lib.rs whose
//!      body is a real branch-free doubler, and EVERY textual definition of
//!      `double_first`/`double_last` calls the helper and *only* the
//!      helper produces the returned value — no inline doubling, and no
//!      decoy call whose result is thrown away, is left behind.
//!   3. The produced crate is honestly runnable: it builds, and running its
//!      own test suite (not just this file) succeeds.
//!
//! The structural checks are textual (include_str! + a noise-stripping
//! tokenizer), so their soundness rests on one invariant: THE COMPILED CODE
//! MUST BE THE SCANNED TEXT. A previous revision of this spec was defeated
//! three times by breaking exactly that invariant — `macro_rules!` arms
//! spelling the real callers as `fn $name` (so `fn double_first` never
//! appears at a compiled definition site, and the inline `n * 2` inside the
//! macro body is never inspected), clean decoy caller definitions parked in
//! never-compiled `#[cfg(any())]` modules (satisfying "every textual def
//! must pass" vacuously), and a dead-code `fn double` satisfying the helper
//! scan (nothing checked reachability). That revision closed the
//! text/compilation split wholesale via `no_textual_evasion_channels` (no
//! macros, no non-`#[cfg(test)]` cfg, no include!/path!/env!/unsafe/outline
//! modules, and a Cargo.toml that can't redirect the lib path, add
//! dependencies, or add a build script) — every compiled `fn double_first`/
//! `fn double_last` is guaranteed to be a textual definition this file
//! scans.
//!
//! THIS revision closes two more gaps found against that hardened version,
//! neither of which is a text/compilation split:
//!
//!   * A **grading-scope** gap, found twice independently. This repo's
//!     grading harness (`scripts/eval/ratchet.sh`) runs the produced crate
//!     through *exactly* `cargo test --test grade_spec`. That command links
//!     `src/lib.rs` as a plain (non-`--cfg test`) rlib dependency of this
//!     one integration-test binary — the crate's own pre-existing
//!     `#[cfg(test)] mod tests { ... }` (the seed's `double_first_works` /
//!     `double_last_works`) is never compiled, let alone run, by that
//!     invocation. A crew can do a fully genuine refactor and then quietly
//!     corrupt its own unit-test assertion (e.g. `Some(6)` -> `Some(999)`);
//!     every check that only imports the public API or scans src/lib.rs's
//!     *text* is blind to it, even though "the crate still compiles and the
//!     workspace tests still pass unchanged" is a listed goal criterion and
//!     is now genuinely false. `workspace_tests_still_pass` below closes
//!     this from *inside* the spec: it shells out to `cargo test --lib`
//!     (never bare `cargo test`, which would recursively re-invoke this
//!     very binary) against the same produced crate and requires it to
//!     exit successfully. A crew that deletes the seed's tests outright
//!     still passes (0 tests is a vacuous success — the prompt only asks
//!     that nothing be left broken, and the behavioral checks above already
//!     cover correctness directly), but a crew that keeps and corrupts them
//!     is caught.
//!
//!   * A **discarded-call** gap: a body can satisfy every textual
//!     "`double(` is called" check while never using the call's result —
//!     e.g. `v.first().map(|&n| { let _ = double(n); n - (0 - n) })`. The
//!     helper is genuine and really called (so `no_textual_evasion_channels`
//!     and the helper-genuineness check pass honestly), and `n - (0 - n)`
//!     is a real, denylist-evading inline doubling (no `*`, `+`, `<<`, or
//!     `-(-` substring) that the *actual* return value flows through
//!     instead. Closing every possible inline-arithmetic spelling by name
//!     is an unbounded denylist; instead this revision enforces that each
//!     caller body is a **single tail expression** — no `let` bindings and
//!     no `;` — which is exactly what a plain "replace `n * 2` with
//!     `double(n)`" edit produces, and is exactly what statement-sequencing
//!     a discarded call before a separately recomputed value requires
//!     syntactically. A residual is accepted and documented at the bottom
//!     of this file rather than chased indefinitely.
//!
//! Comments AND string/char literals are stripped before the structural
//! checks, so commented-out code can neither satisfy, trip, nor skew them.

use extract_helper::{double_first, double_last};

const LIB_SRC: &str = include_str!("../src/lib.rs");
const MANIFEST: &str = include_str!("../Cargo.toml");

// ---------------------------------------------------------------------------
// 1. Behavior unchanged — as a property over many inputs, not a finite oracle
// ---------------------------------------------------------------------------

#[test]
fn behavior_fixed_cases() {
    assert_eq!(double_first(&[3, 1, 4]), Some(6));
    assert_eq!(double_last(&[3, 1, 4]), Some(8));
    assert_eq!(double_first(&[]), None, "double_first(&[]) must be None");
    assert_eq!(double_last(&[]), None, "double_last(&[]) must be None");
    assert_eq!(double_first(&[-5, 7]), Some(-10), "negatives must survive");
    assert_eq!(double_last(&[7, -5]), Some(-10), "negatives must survive");
    assert_eq!(double_first(&[0]), Some(0), "zero must survive");
    assert_eq!(double_last(&[0]), Some(0), "zero must survive");
    assert_eq!(double_first(&[9]), Some(18), "single element: first == last");
    assert_eq!(double_last(&[9]), Some(18), "single element: first == last");
}

/// Exhaustive band around zero: any lookup-table or identity-fallback
/// "helper" that memorized a finite probe set dies here immediately.
#[test]
fn behavior_exhaustive_band() {
    for k in -65_536i32..=65_536 {
        let want = Some(k * 2);
        assert_eq!(double_first(&[k]), want, "double_first(&[{k}])");
        assert_eq!(double_last(&[k]), want, "double_last(&[{k}])");
        assert_eq!(double_first(&[k, 7, -7]), want, "double_first(&[{k},7,-7])");
        assert_eq!(double_last(&[-7, 7, k]), want, "double_last(&[-7,7,{k}])");
    }
}

/// Wide-range property: deterministic LCG samples across the full
/// doubling-safe i32 range (|k| < 2^30), plus the half-range extremes.
#[test]
fn behavior_wide_range_property() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..20_000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let k = ((state >> 32) as u32 as i32) % (1 << 30);
        let want = Some(k * 2);
        assert_eq!(double_first(&[k, 1]), want, "double_first(&[{k}, 1])");
        assert_eq!(double_last(&[1, k]), want, "double_last(&[1, {k}])");
    }
    for k in [
        i32::MAX / 2,
        i32::MAX / 2 - 1,
        i32::MIN / 2,
        i32::MIN / 2 + 1,
        1,
        -1,
        2,
        10,
        100,
        12_345_678,
        -12_345_678,
    ] {
        assert_eq!(double_first(&[k]), Some(k * 2), "double_first(&[{k}])");
        assert_eq!(double_last(&[k]), Some(k * 2), "double_last(&[{k}])");
    }
}

// ---------------------------------------------------------------------------
// 2. Structural checks (on comment- and literal-stripped src/lib.rs)
// ---------------------------------------------------------------------------

/// Strip `//` line comments, (nested) `/* */` block comments, `"…"` string
/// literals (incl. `r"…"` / `r#"…"#` raw strings), and `'x'` char literals
/// (lifetimes are preserved). Commented-out code can neither satisfy nor
/// trip the structural checks, and braces inside literals cannot skew the
/// brace-depth accounting.
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
        let right_ok = s[i + kw.len()..].chars().next().map_or(true, |c| !is_ident(c));
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

/// ALL indices of `fn <name>` definitions (not calls, not longer idents).
/// Requiring every textual definition to pass — combined with
/// `no_textual_evasion_channels`, which guarantees every COMPILED definition
/// is textual — is what makes the textual scan sound.
fn find_fn_defs(s: &str, name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, _) in s.match_indices(name) {
        let rest = &s[i + name.len()..];
        if rest.chars().next().map_or(true, is_ident) {
            continue; // double_first_works, redouble, …
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

/// The attribute/visibility prefix of the item whose `fn` keyword starts at
/// `fn_kw_start`: the text since the previous item ended (`}` or `;`).
fn item_prefix(s: &str, fn_kw_start: usize) -> &str {
    let upto = &s[..fn_kw_start];
    let cut = upto
        .rfind(|c| c == '}' || c == ';')
        .map(|p| p + 1)
        .unwrap_or(0);
    &upto[cut..]
}

/// THE soundness gate for every other structural check: the compiled code
/// must be exactly the text this spec scans. Each ban below closes a channel
/// by which compiled definitions can diverge from src/lib.rs's visible text.
/// None of these constructs has any legitimate use in this one-file,
/// dependency-free extract-a-helper refactor.
#[test]
fn no_textual_evasion_channels() {
    let src = strip_noise(LIB_SRC);
    let flat = no_ws(&src);

    // --- src/lib.rs -------------------------------------------------------

    // Macros can spell `fn $name` so the compiled callers never appear as
    // `fn double_first` in the text (the exact trick that defeated the
    // previous spec revision, three ways). This refactor needs no macros.
    assert!(
        !contains_word(&src, "macro_rules"),
        "src/lib.rs must not define macros (`macro_rules!`): macro-emitted \
         functions hide their tokens from grading, and this refactor needs \
         no macros"
    );

    // `#[cfg(...)]` other than the seed's own `#[cfg(test)]` can compile
    // text out, turning caller definitions into never-compiled decoys.
    for (i, _) in flat.match_indices("#[cfg") {
        assert!(
            flat[i..].starts_with("#[cfg(test)]"),
            "src/lib.rs must not use #[cfg(...)] / #[cfg_attr(...)] other \
             than the seed's own `#[cfg(test)]` on the tests module: \
             conditional compilation turns graded text into decoys"
        );
    }

    // Code pulled from other files or generated at build time is invisible
    // to this scan; all compiled code must be inline text in src/lib.rs.
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

    // Outline modules (`mod x;`) move compiled code into unscanned files.
    // Inline modules (`mod x { … }`) are fine: their text is scanned.
    for (i, _) in src.match_indices("mod") {
        let left_ok = i == 0 || !is_ident(src[..i].chars().last().unwrap());
        let right = &src[i + 3..];
        if !left_ok || right.chars().next().map_or(true, is_ident) {
            continue; // part of a longer identifier
        }
        let after_ident: &str = right
            .trim_start()
            .trim_start_matches(is_ident)
            .trim_start();
        assert!(
            after_ident.starts_with('{'),
            "src/lib.rs must not declare outline modules (`mod x;`): all \
             compiled code must be inline text in src/lib.rs"
        );
    }

    // --- Cargo.toml -------------------------------------------------------
    // The spec links `extract_helper` and scans src/lib.rs; the manifest
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
         [build-dependencies]: this refactor needs no crates"
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
fn helper_is_top_level_private_genuine_doubler() {
    let src = strip_noise(LIB_SRC);

    // Exactly ONE `fn double` definition may exist, and it must carry the
    // pinned signature `fn double(n: i32) -> i32 { … }` at brace depth 0.
    // Uniqueness closes the "callers call a different, shadowing `double`
    // while a decorative one sits at top level" family outright.
    let defs = find_fn_defs(&src, "double");
    assert_eq!(
        defs.len(),
        1,
        "src/lib.rs must define exactly one `fn double` (found {}): the \
         single extracted helper, no shadowing doubles",
        defs.len()
    );
    let i = defs[0];

    // Pinned signature (whitespace-insensitive, tokens exact).
    let rest = &src[i + "double".len()..];
    let sig = no_ws(&rest[..rest.len().min(64)]);
    assert!(
        sig.starts_with("(n:i32)->i32{"),
        "`fn double` must have the exact pinned signature \
         `fn double(n: i32) -> i32`"
    );
    assert_eq!(
        depth_at(&src, i),
        0,
        "`fn double` must be a top-level function: not a method in an impl \
         block, not nested in another fn, not inside a mod"
    );

    // Privacy: the token before `fn` must not be `pub` / `pub(...)`.
    let before = src[..i].trim_end();
    let fn_kw = before.len() - 2;
    let prefix = item_prefix(&src, fn_kw);
    let prev_tok = prefix.split_whitespace().last().unwrap_or("");
    assert!(
        prev_tok != "pub" && !prev_tok.starts_with("pub("),
        "`fn double` must be private, found `{prev_tok} fn double`"
    );

    // Belt-and-braces: no attribute on the helper (cfg is banned globally;
    // this also rejects a cfg_attr'd or otherwise-decorated decoy).
    assert!(
        !prefix.contains("#[cfg"),
        "`fn double` must not be behind a #[cfg(...)] attribute"
    );

    // Genuineness of the helper BODY: it must actually double, not memorize
    // the grader's inputs. A real extracted helper is a tiny, branch-free
    // doubling expression.
    let body = body_after(&src, i).expect("could not extract the body of `fn double`");
    let b = no_ws(body);
    assert!(
        b.len() <= 160,
        "`fn double`'s body is suspiciously large ({} chars); an extracted \
         doubling helper is a one-liner, not a lookup table",
        b.len()
    );
    for kw in ["match", "if", "else", "loop", "while", "for"] {
        assert!(
            !contains_word(body, kw),
            "`fn double`'s body must be a branch-free doubling expression; \
             found `{kw}` (lookup tables / input-conditional helpers are \
             not the extracted `n * 2`)"
        );
    }
    assert!(
        !b.contains("=>"),
        "`fn double`'s body must not contain match arms"
    );
    let forms = [
        "n*2",
        "2*n",
        "n+n",
        "n<<1",
        "n.wrapping_mul(2)",
        "n.checked_mul(2)",
        "n.saturating_mul(2)",
    ];
    assert!(
        forms.iter().any(|f| b.contains(f)),
        "`fn double`'s body must contain a recognized doubling of `n` \
         (e.g. `n * 2`); body was: {body}"
    );
}

#[test]
fn callers_use_helper_and_have_no_inline_doubling() {
    let src = strip_noise(LIB_SRC);

    for name in ["double_first", "double_last"] {
        let defs = find_fn_defs(&src, name);
        assert!(!defs.is_empty(), "src/lib.rs must still define `fn {name}`");

        // EVERY textual definition must be a clean, helper-calling body.
        // `no_textual_evasion_channels` guarantees every compiled definition
        // is one of these (no macro-emitted `fn $name`, no cfg'd decoys, no
        // out-of-file code), so this check now covers the code that runs.
        for def in defs {
            let body = body_after(&src, def)
                .unwrap_or_else(|| panic!("could not extract the body of `fn {name}`"));
            let b = no_ws(body);

            // Single-tail-expression discipline: no `let` bindings and no
            // `;` anywhere in the body. This is exactly what a plain
            // "replace `n * 2` with `double(n)`" edit produces (the
            // reference solution's bodies are one bare expression each),
            // and it is exactly what a *discarded-call decoy* needs to
            // exist at all: `let _ = double(n); n - (0 - n)` (or the
            // let-free `double(n); n - (0 - n)`) requires a statement
            // separator to sequence "call the helper and throw the result
            // away" before "separately recompute the value some other,
            // denylist-evading way". A body that must be a single
            // expression cannot sequence a discard before a substitute
            // computation, so the only way left to make `double(` appear
            // in it is to actually use its result.
            assert!(
                !contains_word(body, "let"),
                "`{name}`'s body must not use `let` bindings — it must be a \
                 single expression that calls `double`, not a sequence of \
                 statements (a `let _ = double(n);` discard followed by a \
                 separately recomputed value would defeat the point of the \
                 helper while still textually \"calling\" it); body was: \
                 {body}"
            );
            assert!(
                !body.contains(';'),
                "`{name}`'s body must be a single tail expression (no `;`) \
                 that calls `double` — statement-sequencing a discarded \
                 `double(n);` before a separately recomputed value is the \
                 same evasion `let` bindings enable; body was: {body}"
            );

            // No inline doubling — named forms first for a clear message…
            for bad in ["n*2", "2*n", "n+n", "n<<1"] {
                assert!(
                    !b.contains(bad),
                    "`{name}` still doubles inline (`{bad}`) instead of \
                     calling the `double` helper"
                );
            }
            // …then the operator/idiom bans that close alias, method, and
            // iterator evasions (`let m = n; m + m`, `k.wrapping_mul(2)`,
            // `n.checked_add(n)`, shifts, repeat/sum chains, `use`-shadowed
            // doubles). A genuine caller body
            // (`v.first().map(|&n| double(n))` and friends) uses none of
            // these.
            for bad in [
                "+", "<<", "mul", "pow", "shl", "sub", "neg", "rotate", "-(-", "add", "sum",
                "product", "fold", "count", "repeat", "wrapping", "checked", "saturating",
                "overflowing", "transmute",
            ] {
                assert!(
                    !b.contains(bad),
                    "`{name}`'s body contains `{bad}` — doubling must happen \
                     inside the `double` helper, not inline; body was: {body}"
                );
            }
            // A bare `-` is otherwise allowed (e.g. unary negation is not
            // itself doubling), but *combined with* the no-`let`/no-`;`
            // single-tail-expression rule above, there is no way left to
            // route a bare `-` into an alternate doubling computation that
            // both compiles and coexists with a genuine `double(n)` call in
            // the same single expression without also using one of the
            // already-banned tokens above (see the doc comment's discussion
            // of the discarded-call gap this revision closes).
            for (idx, _) in b.match_indices('*') {
                let prev = if idx == 0 { '(' } else { b[..idx].chars().last().unwrap() };
                assert!(
                    !(is_ident(prev) || prev == ')' || prev == ']'),
                    "`{name}`'s body multiplies inline (`…{prev}*…`) instead \
                     of calling the `double` helper; body was: {body}"
                );
            }
            // No `use` inside the body (shadowing `double` with an import).
            assert!(
                !contains_word(body, "use"),
                "`{name}`'s body must not import a different `double`"
            );

            // The result must flow through the helper: a bare call
            // `double(…)` with a clean left boundary (rejects `x.double(…)`
            // and `T::double(…)`), or the function passed by name to `map`.
            let clean_left = |idx: usize| -> bool {
                idx == 0 || {
                    let c = b[..idx].chars().last().unwrap();
                    !is_ident(c) && c != '.' && c != ':'
                }
            };
            let calls_helper = b
                .match_indices("double(")
                .any(|(idx, _)| clean_left(idx))
                || b.contains("map(double)");
            assert!(
                calls_helper,
                "`{name}` must compute its result by calling the `double` \
                 helper (e.g. `double(n)`); body was: {body}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. The produced crate is honestly runnable, not just this file
// ---------------------------------------------------------------------------

/// Closes a grading-*scope* gap, found independently twice: this repo's
/// harness (`scripts/eval/ratchet.sh`) grades a crew's work by running
/// *exactly* `cargo test --test grade_spec` in the produced tree. That
/// command links `src/lib.rs` as a plain rlib dependency of this one
/// integration-test binary — it never passes `--cfg test`, so the crate's
/// own pre-existing `#[cfg(test)] mod tests { ... }` (the seed's
/// `double_first_works` / `double_last_works`) is never compiled or run by
/// it. A crew can do a fully genuine, non-gamed refactor (real helper, real
/// calls, nothing else in this file trips) and then quietly corrupt its own
/// unit-test assertion — every check above, scoped to the public API and
/// src/lib.rs's *text*, is structurally blind to that, even though "the
/// crate still compiles and the workspace tests still pass unchanged" is a
/// listed goal criterion and would now be genuinely false.
///
/// Close it from inside the spec: shell out to `cargo test --lib` (which
/// DOES compile with `--cfg test`, so it exercises the crate's own unit
/// tests) against the very crate this file was dropped into, and require it
/// to succeed. Deliberately `--lib`, never bare `cargo test` — the latter
/// would rebuild and re-run every target *including this integration-test
/// binary itself*, recursing into `workspace_tests_still_pass` forever. A
/// crew that deletes the seed's unit tests outright still passes here (0
/// tests is a vacuous success: the prompt's actual ask is "don't leave the
/// workspace broken", and correctness itself is already covered directly by
/// the `behavior_*` tests above), but a crew that keeps and corrupts them is
/// caught.
#[test]
fn workspace_tests_still_pass() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .unwrap_or_else(|_| format!("{manifest_dir}/target"));
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
        .output()
        .expect("failed to invoke `cargo test --lib` to verify the crate's own tests");

    assert!(
        output.status.success(),
        "the produced crate's own tests (`cargo test --lib`, which runs any \
         #[cfg(test)] mod in src/lib.rs) do not pass — \"the workspace \
         tests still pass unchanged\" is violated even though `cargo test \
         --test grade_spec` alone would never see it:\n--- stdout ---\n{}\n\
         --- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// ---------------------------------------------------------------------------
// Accepted residual (documented, not chased further — see the strategy this
// spec was authored against): a caller body could in principle spell an
// inline doubling with tokens outside every list above (e.g. via a helper
// trait method with an unrelated name, or arithmetic hidden behind a
// locally-defined `Index`/`Add` impl) while *also* keeping a live,
// result-flowing call to `double`. Such a construct would need to avoid
// EVERY banned token, stay a single tail expression, and still make
// `double(n)`'s return value the one that flows out — at that point it is
// simply calling `double` twice under two names, which is not a meaningful
// evasion of "compute the result by calling `double`". Kept as a documented
// non-goal rather than an unbounded token denylist.
