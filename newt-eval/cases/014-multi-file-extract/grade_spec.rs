// PROVENANCE: authored by the grade-spec-author workflow.
// strategy=hybrid
// Survived 3 red-team rounds (8 valid games defeated).
// Certified: honest-solution PASS, unmodified-seed FAIL, corpus replay all-FAIL.
//
//! Hidden structural + behavioral spec for 014-multi-file-extract.
//!
//! Dropped into the produced tree at grading time as `tests/grade_spec.rs`
//! and run via `cargo test --test grade_spec`; the agent under eval never
//! sees this file.
//!
//! The naive case.toml evaluators (`pattern_match` on `"mod util;"` /
//! `"util::multiply"`, `rust_compiles`, `tests_pass`) can all be satisfied
//! without the helper ever actually moving: the literal strings can live in
//! a comment, a decoy `util.rs` can sit next to an untouched `lib.rs`, the
//! visibility can drift to `pub`, or the whole thing can be an inline
//! `mod util { ... }` block. This spec checks the actual structure of the
//! move, not just substring presence — and it is hardened against seven
//! concrete attacks found against earlier drafts:
//!
//!   1. **TOCTOU / self-rewriting artifact ("ELF constructor") attack.**
//!      A submission can leave `lib.rs` genuinely un-refactored and simply
//!      prepend a `#[link_section = ".init_array"]` constructor that
//!      rewrites `src/lib.rs` / creates `src/util.rs` on disk the instant
//!      the process starts — before any `#[test]` body gets control,
//!      including this very `grade_spec` binary.
//!
//!      Fix: `src/lib.rs` and `Cargo.toml` are captured via `include_str!`,
//!      resolved at *compile time*. A runtime constructor cannot rewrite
//!      what the compiler already baked into the binary as a string
//!      constant, and (for `mod util;` to make the crate compile at all)
//!      `src/util.rs` must already exist on disk *before* compilation
//!      starts — a runtime constructor cannot retroactively satisfy that.
//!
//!   2. **Renamed dead-code duplicate.** The move is done correctly but the
//!      old multiplying logic is *also* left behind in `lib.rs` under a new
//!      name/parameter identifiers. An earlier draft of this spec tried to
//!      catch this by enumerating literal body shapes (`p0 * p1`,
//!      `.wrapping_mul(..)`, etc.) — that enumeration was proven
//!      incomplete: `.checked_mul(y).unwrap_or(u32::MAX)` and
//!      `.checked_mul(y).unwrap()` compute the identical answer for every
//!      non-overflowing input but don't match any of the fixed candidate
//!      strings, so a `#[allow(dead_code)] fn legacy_combine(x, y) -> u32 {
//!      x.checked_mul(y).unwrap_or(u32::MAX) }` (or any other differently
//!      spelled twin) sailed through undetected. Enumerating arithmetic
//!      idioms is an unwinnable arms race. Fix: `is_semantic_multiply_clone`
//!      replaces syntax-matching with *behavior*-matching: it compiles
//!      every other two-argument function found anywhere in `lib.rs` (any
//!      name, any depth, any arithmetic spelling) into a standalone
//!      throwaway program and runs it against several small,
//!      non-overflowing input pairs, comparing to the true product. Any
//!      function reproducing `a * b` for all of them is flagged as a
//!      behavioral duplicate, closing the whole class of "rename + respell
//!      the arithmetic" bypasses in one shot.
//!
//!   3. **Nested-module-behind-a-re-export "top-level" spoof.** Fixed via
//!      `depth_of_first_match`, which tracks actual brace-nesting depth
//!      character-by-character (string-literal-aware) and requires the
//!      `pub(crate) fn multiply` signature to occur at depth 0, catching a
//!      `mod inner { ... }` wrapper regardless of how its contents are
//!      indented.
//!
//!   4. **Shadow-import + wrong decoy.** Fixed by banning the bare token
//!      sequence `fn multiply(` anywhere in `lib.rs` (any depth, any
//!      params), plus banning `let`/`const`/`static`/`as` rebindings of the
//!      name `multiply`.
//!
//!   5. **Self-erasing build.rs.** A build.rs rewrites lib.rs/util.rs to
//!      look compliant then deletes itself before any test observes it.
//!      Fixed by `no_build_script_artifacts_ever_existed`, which scans
//!      `target/{debug,release}/build/` for forensic evidence (Cargo
//!      creates `<pkg>-<hash>/` entries there whenever a build script ran,
//!      and never retroactively deletes them). Because this evidence must
//!      survive the whole grading run, no test in this file may ever run
//!      `cargo clean` — see `touch_to_force_rebuild` below, which forces a
//!      fresh compile via mtime invalidation instead, so it cannot
//!      accidentally destroy this forensic trail.
//!
//!   6. **Decorative/unused `use` + fully-qualified bypass call.** `lib.rs`
//!      keeps a real `use util::multiply;` line but `area`'s body calls
//!      the fully-qualified `util::multiply(w, h)` instead of the bare
//!      imported name — compiles identically either way, and the compiler
//!      itself emits `warning: unused import: \`util::multiply\``, so the
//!      import is decorative. A substring check for `multiply(` in area's
//!      body is fooled because `util::multiply(` contains `multiply(` as a
//!      trailing substring. Fix, two layers: (a) ban any `::multiply(`
//!      occurrence in area's body, and (b)
//!      `use_util_multiply_import_is_not_dead_code` forces a fresh rebuild
//!      (via mtime-touch, not `cargo clean`) and asserts no
//!      `unused_imports` warning mentioning `multiply` was emitted — the
//!      general, compiler-verified counterpart to (a).
//!
//!   7. **Gutted own test suite, invisible to `cargo test --test
//!      grade_spec` alone**, since that never builds `#[cfg(test)]` code.
//!      Fixed by `crate_own_test_suite_actually_passes`, which shells out
//!      to `cargo test --lib` for real.
//!
//! All `cargo` invocations in this file share a mutex (`CARGO_LOCK`) so
//! concurrently-running `#[test]` functions can't race each other's view
//! of `target/`.

use multi_file_extract::area;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static CARGO_LOCK: Mutex<()> = Mutex::new(());

const LIB_RS: &str = include_str!("../src/lib.rs");
const CARGO_TOML: &str = include_str!("../Cargo.toml");

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = crate_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("expected to read {} from crate under test: {e}", path.display()))
}

fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if bytes[i] == b'"' {
            out.push('"');
            i += 1;
            while i < n && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < n {
                    out.push(bytes[i] as char);
                    out.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < n {
                out.push('"');
                i += 1;
            }
        } else if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(n);
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn strip_ws(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

fn depths_for_nows(stripped: &str) -> (String, Vec<i32>) {
    let bytes = stripped.as_bytes();
    let n = bytes.len();
    let mut nows = String::with_capacity(n);
    let mut depths: Vec<i32> = Vec::with_capacity(n);
    let mut depth: i32 = 0;
    let mut i = 0usize;
    while i < n {
        let c = bytes[i] as char;
        if c == '"' {
            if !c.is_whitespace() {
                nows.push(c);
                depths.push(depth);
            }
            i += 1;
            while i < n {
                let sc = bytes[i] as char;
                if sc == '\\' && i + 1 < n {
                    if !sc.is_whitespace() {
                        nows.push(sc);
                        depths.push(depth);
                    }
                    let esc = bytes[i + 1] as char;
                    if !esc.is_whitespace() {
                        nows.push(esc);
                        depths.push(depth);
                    }
                    i += 2;
                    continue;
                }
                if !sc.is_whitespace() {
                    nows.push(sc);
                    depths.push(depth);
                }
                i += 1;
                if sc == '"' {
                    break;
                }
            }
            continue;
        }
        if !c.is_whitespace() {
            nows.push(c);
            depths.push(depth);
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
        }
        i += 1;
    }
    (nows, depths)
}

fn depth_of_first_match(stripped: &str, needle_nows: &str) -> Option<i32> {
    let (nows, depths) = depths_for_nows(stripped);
    let idx = nows.find(needle_nows)?;
    depths.get(idx).copied()
}

fn has_toplevel_undecorated_line(comment_stripped: &str, needle: &str) -> bool {
    let lines: Vec<&str> = comment_stripped.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with(needle) && *line == line.trim_start() {
            let mut j = idx;
            let mut blocked = false;
            while j > 0 {
                j -= 1;
                let prev = lines[j].trim();
                if prev.is_empty() {
                    continue;
                }
                if prev.starts_with("#[cfg(") || prev.starts_with("#[path") {
                    blocked = true;
                }
                break;
            }
            if !blocked {
                return true;
            }
        }
    }
    false
}

fn extract_block(src: &str, marker: &str) -> Option<String> {
    let start = src.find(marker)?;
    let after = &src[start..];
    let brace_start = after.find('{')?;
    let bytes = after.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(brace_start) {
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    Some(after[brace_start + 1..end].to_string())
}

struct FnSpan {
    name: String,
    param_count: usize,
    text: String,
}

fn extract_fn_spans(src: &str) -> Vec<FnSpan> {
    let bytes = src.as_bytes();
    let mut spans = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = src[search_from..].find("fn ") {
        let fn_kw_start = search_from + rel;
        let boundary_ok = fn_kw_start == 0 || {
            let prev = bytes[fn_kw_start - 1] as char;
            !(prev.is_alphanumeric() || prev == '_')
        };
        if !boundary_ok {
            search_from = fn_kw_start + 3;
            continue;
        }
        let after_kw = fn_kw_start + 3;
        let name_end = src[after_kw..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map(|i| after_kw + i)
            .unwrap_or(src.len());
        let name = src[after_kw..name_end].to_string();
        if name.is_empty() {
            search_from = fn_kw_start + 3;
            continue;
        }
        let paren_open = match src[name_end..].find('(') {
            Some(i) => name_end + i,
            None => {
                search_from = fn_kw_start + 3;
                continue;
            }
        };
        let between = &src[name_end..paren_open];
        if !between
            .chars()
            .all(|c| c.is_whitespace() || c.is_alphanumeric() || "<>,'_:".contains(c))
        {
            search_from = fn_kw_start + 3;
            continue;
        }
        let mut depth = 0i32;
        let mut params_end = None;
        for (i, &b) in bytes.iter().enumerate().skip(paren_open) {
            if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
                if depth == 0 {
                    params_end = Some(i);
                    break;
                }
            }
        }
        let params_end = match params_end {
            Some(v) => v,
            None => break,
        };
        let params_str = &src[paren_open + 1..params_end];
        let param_count = params_str
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .count();
        let brace_rel = match src[params_end..].find('{') {
            Some(i) => params_end + i,
            None => {
                search_from = fn_kw_start + 3;
                continue;
            }
        };
        if src[params_end..brace_rel].contains(';') {
            search_from = fn_kw_start + 3;
            continue;
        }
        let mut depth2 = 0i32;
        let mut body_end = None;
        for (i, &b) in bytes.iter().enumerate().skip(brace_rel) {
            if b == b'{' {
                depth2 += 1;
            } else if b == b'}' {
                depth2 -= 1;
                if depth2 == 0 {
                    body_end = Some(i);
                    break;
                }
            }
        }
        let body_end = match body_end {
            Some(v) => v,
            None => break,
        };
        let text = src[fn_kw_start..=body_end].to_string();
        spans.push(FnSpan {
            name,
            param_count,
            text,
        });
        search_from = body_end + 1;
    }
    spans
}

fn unique_scratch_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&dir).expect("failed to create scratch dir for semantic clone probe");
    dir
}

fn is_semantic_multiply_clone(span: &FnSpan) -> Option<bool> {
    let dir = unique_scratch_dir("grade-spec-clone-probe");
    let src_path = dir.join("probe.rs");
    let bin_path = dir.join("probe_bin");

    let harness_template = r#"
fn main() {
    let pairs: [(i128, i128); 10] = [
        (0, 0), (1, 1), (0, 5), (5, 0), (2, 3),
        (3, 4), (7, 8), (9, 9), (6, 7), (11, 4),
    ];
    let mut all_match = true;
    for &(a, b) in pairs.iter() {
        let got = __CANDIDATE_NAME__(a as _, b as _);
        let got_i128: i128 = got as i128;
        let expected: i128 = a * b;
        if got_i128 != expected {
            all_match = false;
        }
    }
    if all_match {
        println!("SEMANTIC_MULTIPLY_CLONE_DETECTED");
    } else {
        println!("NOT_A_CLONE");
    }
}
"#;
    let harness = harness_template.replace("__CANDIDATE_NAME__", &span.name);
    let probe_src = format!("{}\n\n{harness}", span.text);
    if std::fs::write(&src_path, probe_src).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    let compile = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-o"])
        .arg(&bin_path)
        .arg(&src_path)
        .output();
    let compile = match compile {
        Ok(o) => o,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
    };
    if !compile.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    let run = Command::new(&bin_path).output();
    let result = run.ok().map(|o| {
        let stdout = String::from_utf8_lossy(&o.stdout).to_string();
        stdout.contains("SEMANTIC_MULTIPLY_CLONE_DETECTED")
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn parse_test_result(stdout: &str) -> Option<(u32, u32)> {
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("test result:") {
            let passed = num_before(rest, "passed")?;
            let failed = num_before(rest, "failed")?;
            return Some((passed, failed));
        }
    }
    None
}

fn num_before(s: &str, keyword: &str) -> Option<u32> {
    let idx = s.find(keyword)?;
    let before = s[..idx].trim_end();
    let digits: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let digits: String = digits.chars().rev().collect();
    digits.parse::<u32>().ok()
}

fn cargo_package_name() -> String {
    let stripped = strip_comments(CARGO_TOML);
    for line in stripped.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                if let Some(rest) = rest.strip_prefix('"') {
                    if let Some(end) = rest.find('"') {
                        return rest[..end].to_string();
                    }
                }
            }
        }
    }
    panic!("could not find a `name = \"...\"` field in Cargo.toml:\n{CARGO_TOML}");
}

fn target_dir() -> PathBuf {
    if let Ok(v) = std::env::var("CARGO_TARGET_DIR") {
        PathBuf::from(v)
    } else {
        crate_root().join("target")
    }
}

fn touch_to_force_rebuild(path: &Path) {
    if let Ok(content) = std::fs::read_to_string(path) {
        let _ = std::fs::write(path, content);
    }
}

#[test]
fn util_rs_exists_and_genuinely_defines_multiply() {
    let util_path = crate_root().join("src/util.rs");
    assert!(
        util_path.exists(),
        "src/util.rs must exist as its own file (the helper must be moved into a real module file, \
         not an inline `mod util {{ ... }}` block in lib.rs)"
    );
    let raw = read("src/util.rs");
    let stripped = strip_comments(&raw);
    let nows = strip_ws(&stripped);

    let sig = "fnmultiply(a:u32,b:u32)->u32";
    let idx = nows.find(sig).unwrap_or_else(|| {
        panic!(
            "src/util.rs must define `fn multiply(a: u32, b: u32) -> u32` as real (non-comment) \
             source; found none. File contents:\n{raw}"
        )
    });
    assert!(
        nows[..idx].ends_with("pub(crate)"),
        "the `multiply` defined in src/util.rs must be visible as exactly `pub(crate) fn multiply`, \
         not `pub fn multiply`, not private-in-module, and not reached via a `use`/re-export shell \
         game. File contents:\n{raw}"
    );

    let depth = depth_of_first_match(&stripped, sig).unwrap_or_else(|| {
        panic!("could not re-locate `fn multiply` signature in src/util.rs for depth check:\n{raw}")
    });
    assert_eq!(
        depth, 0,
        "the `pub(crate) fn multiply` definition in src/util.rs must be a genuine top-level item \
         (brace-nesting depth 0). File contents:\n{raw}"
    );

    assert!(
        has_toplevel_undecorated_line(&stripped, "pub(crate) fn multiply"),
        "the `pub(crate) fn multiply` definition in src/util.rs must not be hidden behind \
         `#[cfg(...)]` or redirected via `#[path = ...]`. File contents:\n{raw}"
    );
}

#[test]
fn lib_rs_has_no_multiply_shadow_or_duplicate() {
    let stripped = strip_comments(LIB_RS);
    let nows = strip_ws(&stripped);

    assert!(
        !nows.contains("fnmultiply(a:u32,b:u32)"),
        "src/lib.rs must no longer contain any `fn multiply(a: u32, b: u32)` definition of its \
         own. File contents:\n{LIB_RS}"
    );

    assert!(
        !nows.contains("fnmultiply("),
        "src/lib.rs must not define ANY function literally named `multiply` anywhere. File \
         contents:\n{LIB_RS}"
    );

    for banned in [
        "letmultiply", "letmutmultiply", "constmultiply", "staticmultiply", "asmultiply",
    ] {
        assert!(
            !nows.contains(banned),
            "src/lib.rs must not locally rebind/alias the name `multiply` (found `{banned}`). \
             File contents:\n{LIB_RS}"
        );
    }

    for span in extract_fn_spans(&stripped) {
        if span.name == "area" || span.name == "multiply" {
            continue;
        }
        if span.param_count != 2 {
            continue;
        }
        if let Some(true) = is_semantic_multiply_clone(&span) {
            panic!(
                "src/lib.rs defines a function `{}` that behaves exactly like the old `multiply` \
                 even though its name/body differs. Offending definition:\n{}\nFull \
                 lib.rs:\n{LIB_RS}",
                span.name, span.text
            );
        }
    }
}

#[test]
fn mod_util_is_a_real_outline_declaration() {
    let stripped = strip_comments(LIB_RS);
    let nows = strip_ws(&stripped);

    assert!(nows.contains("modutil;"), "missing `mod util;`. lib.rs:\n{LIB_RS}");
    assert!(!nows.contains("modutil{"), "must be outline, not inline. lib.rs:\n{LIB_RS}");

    let depth = depth_of_first_match(&stripped, "modutil;")
        .unwrap_or_else(|| panic!("could not re-locate `mod util;`:\n{LIB_RS}"));
    assert_eq!(depth, 0, "`mod util;` must be top-level. lib.rs:\n{LIB_RS}");

    assert!(
        has_toplevel_undecorated_line(&stripped, "mod util;"),
        "`mod util;` must not be cfg-gated/redirected. lib.rs:\n{LIB_RS}"
    );
}

#[test]
fn use_util_multiply_is_real_and_actually_used_by_area() {
    let stripped = strip_comments(LIB_RS);
    let nows = strip_ws(&stripped);

    assert!(nows.contains("useutil::multiply;"), "missing `use util::multiply;`. lib.rs:\n{LIB_RS}");

    let area_body = extract_block(&stripped, "fn area(")
        .unwrap_or_else(|| panic!("could not locate `fn area`:\n{LIB_RS}"));
    let area_body_nows = strip_ws(&area_body);

    assert!(area_body_nows.contains("multiply("), "area must call multiply(...). body:\n{area_body}");
    assert!(!area_body.contains('*'), "area must not inline arithmetic. body:\n{area_body}");
    assert!(
        !area_body_nows.contains("::multiply("),
        "area's call must go through the bare imported name, not a qualified path \
         (util::multiply(...)), which makes the `use` decorative dead code. body:\n{area_body}"
    );
}

#[test]
fn use_util_multiply_import_is_not_dead_code() {
    let _guard = CARGO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    touch_to_force_rebuild(&crate_root().join("src/lib.rs"));

    let output = Command::new(env!("CARGO"))
        .args(["build", "--lib"])
        .current_dir(crate_root())
        .output()
        .expect("failed to invoke `cargo build --lib`");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "cargo build --lib must succeed:\n{combined}");
    let cl = combined.to_lowercase();
    assert!(
        !(cl.contains("unused import") && cl.contains("multiply")),
        "fresh build reported unused_imports re: multiply — the use import must be genuinely \
         used, not bypassed by a qualified-path call. Output:\n{combined}"
    );
}

#[test]
fn area_behavior_is_unchanged() {
    for (w, h) in [(3u32, 4u32), (0, 5), (1, 1), (7, 8), (100, 100), (12, 13)] {
        assert_eq!(area(w, h), w * h, "area({w}, {h}) must still equal w * h");
    }
}

#[test]
fn crate_layout_is_untouched() {
    let nows = strip_ws(&strip_comments(CARGO_TOML));
    assert!(nows.contains("path=\"src/lib.rs\""), "Cargo.toml:\n{CARGO_TOML}");
    assert!(!nows.contains("build="), "no build script allowed. Cargo.toml:\n{CARGO_TOML}");
    assert!(!crate_root().join("build.rs").exists(), "no build.rs allowed");

    let src_dir = crate_root().join("src");
    let mut entries: Vec<String> = std::fs::read_dir(&src_dir)
        .expect("src/ must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["lib.rs".to_string(), "util.rs".to_string()], "no extra src files allowed");
}

#[test]
fn no_build_script_artifacts_ever_existed() {
    let pkg = cargo_package_name();
    let candidates = [pkg.clone(), pkg.replace('-', "_")];
    let tdir = target_dir();

    for profile in ["debug", "release"] {
        let build_dir = tdir.join(profile).join("build");
        let entries = match std::fs::read_dir(&build_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            for cand in &candidates {
                assert!(
                    !fname.starts_with(cand.as_str()),
                    "found build-script cache artifacts at {}/{fname} — a build.rs executed for \
                     this crate at some point during grading, even if it self-deleted afterward.",
                    build_dir.display()
                );
            }
        }
    }
}

#[test]
fn crate_own_test_suite_actually_passes() {
    let stripped = strip_comments(LIB_RS);
    let nows = strip_ws(&stripped);

    assert!(nows.contains("fncomputes_area"), "computes_area test must exist. lib.rs:\n{LIB_RS}");
    assert!(
        nows.contains("assert_eq!(area(3,4),12)"),
        "computes_area's assertion must be intact. lib.rs:\n{LIB_RS}"
    );

    let _guard = CARGO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let output = Command::new(env!("CARGO"))
        .args(["test", "--lib"])
        .current_dir(crate_root())
        .output()
        .expect("failed to invoke `cargo test --lib`");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let combined = format!("{stdout}\n{}", String::from_utf8_lossy(&output.stderr));

    assert!(output.status.success(), "cargo test --lib must pass:\n{combined}");
    let (passed, failed) = parse_test_result(&stdout)
        .unwrap_or_else(|| panic!("could not parse cargo test --lib output:\n{combined}"));
    assert!(passed >= 1 && failed == 0, "got {passed} passed, {failed} failed:\n{combined}");
    assert!(combined.contains("computes_area"), "computes_area must have run:\n{combined}");
}
