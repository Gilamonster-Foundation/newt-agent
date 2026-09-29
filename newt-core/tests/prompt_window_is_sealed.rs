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

/// **#2027, the type-level half of the terminal-taker registration.**
///
/// The registry test (`terminal_taker_registry.rs`) proves both directions of
/// the declaration ↔ acquisition correspondence, but one of those directions
/// is only interesting if the argument is genuinely required. Making the taker
/// optional — a `Default`, an overload, a convenience door — would leave the
/// registry proving a correspondence over a set anyone can opt out of. This is
/// the twin that keeps it honest, in the same shape and the same harness as
/// the seal's three cases above, because it is the same kind of claim: a wrong
/// call does not compile.
#[test]
fn a_prompt_window_cannot_be_acquired_without_declaring_the_taker() {
    assert_compile_fails(
        "tests/ui/suspend_for_prompt_requires_a_taker.rs",
        6,
        &["suspend_for_prompt", "argument"],
    );
}

/// **The probe must resolve what the workspace resolves.**
///
/// The seal above is only as trustworthy as the crate it is compiled in. That
/// crate is generated outside the workspace, so nothing but this copy stops it
/// drifting onto whatever crates.io published most recently — and when it
/// drifts, it fails in the same direction as a real seal violation, which is
/// the worst possible failure mode for a compile-fail test.
///
/// So the pin is asserted rather than assumed. This costs no network and no
/// compile: it checks the scaffolding, not the fixture.
#[test]
fn the_seal_probe_crate_is_pinned_to_the_workspace_lockfile() {
    let temp = tempfile::tempdir().expect("create probe crate");
    write_seal_probe_crate(
        temp.path(),
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/ui/prompt_window_cannot_be_struct_literaled.rs"),
    );

    let seeded = std::fs::read_to_string(temp.path().join("Cargo.lock"))
        .expect("the probe crate carries a lockfile");
    let workspace =
        std::fs::read_to_string(workspace_lockfile()).expect("the workspace carries a lockfile");
    assert_eq!(
        seeded, workspace,
        "the seal probe's lockfile is not the workspace's — its dependencies \
         would resolve fresh from the registry and could fail to build for \
         reasons that have nothing to do with the seal"
    );

    let manifest = std::fs::read_to_string(temp.path().join("Cargo.toml"))
        .expect("the probe crate carries a manifest");
    assert!(
        manifest.contains("[workspace]"),
        "the probe must be its own workspace root, or a TMPDIR inside a cargo \
         workspace would make it use that workspace's lockfile instead of the \
         one written beside it; manifest={manifest}"
    );

    // A copied lockfile alone cannot activate a workspace-level path patch.
    // Compare every declaration, preserving its options and anchoring only
    // paths; the probe must neither omit a patch nor invent an extra one.
    let probe: toml::Value = toml::from_str(&manifest).expect("parse probe manifest");
    let workspace: toml::Value = toml::from_str(
        &std::fs::read_to_string(workspace_root().join("Cargo.toml"))
            .expect("read workspace manifest"),
    )
    .expect("parse workspace manifest");
    let declared = workspace
        .get("patch")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let resolved = probe
        .get("patch")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        resolved.len(),
        declared.len(),
        "patch source inventory differs"
    );
    for (source, packages) in declared {
        let packages = packages.as_table().expect("patch packages are a table");
        let actual = resolved[&source].as_table().expect("probe patch packages");
        assert_eq!(actual.len(), packages.len(), "patch inventory for {source}");
        for (name, definition) in packages {
            let mut expected = definition.clone();
            if let Some(path) = definition.get("path").and_then(toml::Value::as_str) {
                expected["path"] =
                    toml::Value::String(workspace_root().join(path).to_string_lossy().into_owned());
            }
            assert_eq!(actual.get(name), Some(&expected), "patch {source}/{name}");
        }
    }
}

fn assert_compile_fails(fixture: &str, line: usize, needles: &[&str]) {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = manifest_dir.join(fixture);
    let temp = tempfile::tempdir().expect("create compile-fail temp crate");
    write_seal_probe_crate(temp.path(), &fixture_path);

    let target_dir = workspace_root()
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

    // A compile-fail probe that accepts ANY compile error proves nothing: a
    // dependency that fails to build satisfies `!success` exactly as well as a
    // sealed `PromptWindow` does. So the error has to be ATTRIBUTED to the
    // fixture, and the two conclusions get two different messages — "the seal
    // is broken" and "the probe is broken" are opposite findings, and this
    // assertion used to report the second as the first.
    assert!(
        normalized.contains(&normalized_fixture),
        "the probe crate failed to compile, but NOT at {fixture} — so this run \
         proves nothing about the seal. Treat it as a broken harness or a \
         broken build environment, not as a seal violation; stderr={stderr}"
    );
    assert!(
        normalized.contains(&format!(":{line}:")),
        "{fixture} failed to compile, but not at the expected line {line}; \
         stderr={stderr}"
    );
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{fixture} stderr did not contain {needle:?}; stderr={stderr}"
        );
    }
}

/// Scaffold the throwaway crate a fixture is compiled inside.
///
/// **The lockfile is the whole point of this function.** The probe crate lives
/// outside the workspace, so without a lockfile of its own `cargo` resolves its
/// dependencies FRESH FROM THE REGISTRY on every run — the checked-in
/// `Cargo.lock` that pins every other build in this repo does not reach it. The
/// probe therefore compiled against whatever crates.io had published that
/// morning, and a compile-fail test cannot tell a broken dependency from the
/// defect it is watching for: both are "it did not compile".
///
/// That is not hypothetical. tinyvec 1.13.0 shipped a `use alloc::vec::{self,
/// Vec}` that shadows the `vec!` macro it then calls, so the crate does not
/// build without `std`. The workspace was unaffected — its lock pins 1.12.0 —
/// but the unpinned probe picked 1.13.0 up within hours of publication and took
/// `Rust tests`, `Windows build + test` and `Workspace coverage` red on every
/// open PR at once, for a defect in none of them.
///
/// Copying the workspace lock in makes the probe resolve what the workspace
/// resolves. `cargo` keeps every pin it recognises and adds only the probe's own
/// root package. Workspace patch declarations must also be copied: Cargo
/// ignores dependency workspaces' `[patch]` tables, and a lockfile does not
/// activate them. Relative patch paths are anchored to the real workspace.
///
/// The empty `[workspace]` table keeps the probe its own workspace root even if
/// `TMPDIR` points inside a cargo workspace, so the lock beside it is the lock
/// that is used.
fn write_seal_probe_crate(root: &std::path::Path, fixture_path: &std::path::Path) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("create compile-fail src dir");

    let dep = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .to_string_lossy()
        .replace('\\', "/");
    let patches = workspace_patch_manifest();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"prompt-window-seal-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\nnewt-core = {{ path = \"{dep}\" }}\n\n{patches}"
        ),
    )
    .expect("write compile-fail manifest");
    std::fs::copy(workspace_lockfile(), root.join("Cargo.lock"))
        .expect("seed the compile-fail probe with the workspace lockfile");
    std::fs::write(
        src.join("main.rs"),
        format!("include!(r#\"{}\"#);\n", fixture_path.display()),
    )
    .expect("write compile-fail main");
}

/// Reuse the workspace's exact patch resolution without introducing another
/// dependency list. This becomes empty when the temporary patches are removed.
fn workspace_patch_manifest() -> String {
    let root = workspace_root();
    let workspace: toml::Value = toml::from_str(
        &std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest"),
    )
    .expect("parse workspace manifest");
    let Some(mut patches) = workspace.get("patch").cloned() else {
        return String::new();
    };
    for (_, packages) in patches.as_table_mut().expect("patch sources").iter_mut() {
        for (_, definition) in packages.as_table_mut().expect("patch packages").iter_mut() {
            if let Some(path) = definition.get_mut("path") {
                let anchored = root.join(path.as_str().expect("patch path is a string"));
                *path = toml::Value::String(anchored.to_string_lossy().into_owned());
            }
        }
    }
    let mut manifest = toml::Table::new();
    manifest.insert("patch".into(), patches);
    toml::to_string(&manifest).expect("serialize workspace patch declarations")
}

/// The workspace root: the nearest ancestor of this crate that owns a
/// `Cargo.lock`. Both the probe's pins and its shared target directory hang off
/// this one notion rather than off two separate walks up the tree.
fn workspace_root() -> std::path::PathBuf {
    workspace_lockfile()
        .parent()
        .expect("a lockfile has a parent directory")
        .to_path_buf()
}

fn workspace_lockfile() -> std::path::PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|lock| lock.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no Cargo.lock at or above {} — the seal probe has nothing to \
                 pin its dependencies with, and would resolve them fresh from \
                 the registry on every run",
                manifest_dir.display()
            )
        })
}
