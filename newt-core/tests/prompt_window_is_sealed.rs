//! §6.6(a) — **the primary guarantee, checked by the compiler.**
//!
//! The whole design rests on one claim: a prompt that has not suspended the
//! terminal's ephemeral writers *cannot be written*. That holds only if
//! `PromptWindow` is genuinely unforgeable — if a downstream crate can conjure
//! one, every blocking prompt's `&PromptWindow` parameter degrades from a proof
//! obligation into a decorative argument, and the bug walks straight back in at
//! the next `gate.ask` call site.
//!
//! So the seal is not documented, it is *tested*: these cases must FAIL TO
//! COMPILE. A refactor that accidentally makes `PromptWindow` constructible
//! (adding a `Default`, making the field `pub`, exposing `test_stub` outside
//! its feature) turns this test red rather than shipping a silently
//! re-introducible hang.

#[test]
fn a_prompt_window_cannot_be_constructed_outside_newt_core_tty() {
    assert_compile_fails(
        "tests/ui/prompt_window_cannot_be_struct_literaled.rs",
        4,
        &["cannot construct", "PromptWindow", "private fields"],
    );
    assert_compile_fails(
        "tests/ui/prompt_window_has_no_public_constructor.rs",
        5,
        &["no", "named `new`", "PromptWindow"],
    );
    assert_compile_fails(
        "tests/ui/prompt_window_test_stub_is_not_public.rs",
        5,
        &["no", "named `test_stub`", "PromptWindow"],
    );
}

fn assert_compile_fails(fixture: &str, line: usize, needles: &[&str]) {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = manifest_dir.join(fixture);
    let temp = tempfile::tempdir().expect("create compile-fail temp crate");
    let src = temp.path().join("src");
    std::fs::create_dir_all(&src).expect("create compile-fail src dir");

    let dep = manifest_dir.to_string_lossy().replace('\\', "/");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"prompt-window-seal-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nnewt-core = {{ path = \"{dep}\" }}\n"
        ),
    )
    .expect("write compile-fail manifest");
    std::fs::write(
        src.join("main.rs"),
        format!("include!(r#\"{}\"#);\n", fixture_path.display()),
    )
    .expect("write compile-fail main");

    let target_dir = manifest_dir
        .parent()
        .unwrap_or(manifest_dir)
        .join("target")
        .join("prompt-window-seal-probe");
    let output =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["check", "--quiet", "--manifest-path"])
            .arg(temp.path().join("Cargo.toml"))
            .arg("--target-dir")
            .arg(target_dir)
            .output()
            .expect("run cargo check for prompt-window seal fixture");
    assert!(
        !output.status.success(),
        "{fixture} unexpectedly compiled; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let normalized = stderr.replace('\\', "/");
    let normalized_fixture = fixture.replace('\\', "/");
    assert!(
        normalized.contains(&normalized_fixture) && normalized.contains(&format!(":{line}:")),
        "{fixture} did not report the expected fixture location; stderr={stderr}"
    );
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{fixture} stderr did not contain {needle:?}; stderr={stderr}"
        );
    }
}
