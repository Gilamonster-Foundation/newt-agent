//! **The three MSRV sites must keep agreeing with each other.**
//!
//! The floor is declared once, in `[workspace.package] rust-version`, and
//! enforced in three places that all claim in prose to mirror one another:
//! the `msrv` job in `.github/workflows/ci.yml`, the `msrv` recipe in the
//! `justfile`, and `.githooks/pre-push`. Nothing asserted it. They agree today
//! only because a human edited all three by hand.
//!
//! This is the #2161 shape: the MSRV job ran `cargo check --workspace` without
//! `--all-targets`, so a dev-dependency was resolved but never built and its
//! `rust-version` was never enforced. The floor drifted where the check did not
//! look, under a comment stating the check existed to stop exactly that.
//!
//! Parity in this repo has already failed silently twice — see the PR that
//! added this file for the `shell-check` phantom and the `[unix]`/`[windows]`
//! `check` divergence, both out of scope here.
//!
//! # Why this compares the sites to EACH OTHER, not to a literal
//!
//! A ratchet pinning `cargo check --workspace` as a string would have to be
//! edited by the same hand that edits the pipeline, which is the failure it
//! exists to prevent. It would also go red on #2161's own fix. So the command
//! assertion is an EQUALITY between two extracted strings: change one site and
//! it fails; change both the same way and it passes.
//!
//! # Anti-vacuity (#2150)
//!
//! Every guard here can fail in two directions. A renamed job, a moved recipe
//! or a deleted step makes the extractor return `None`, which panics naming
//! what moved — it does not silently match nothing and pass. The sources are
//! embedded with `include_str!` at COMPILE time, so this does no filesystem
//! I/O and stays in the fully-mocked unit tier.

const CI: &str = include_str!("../../.github/workflows/ci.yml");
const JUSTFILE: &str = include_str!("../../justfile");
const HOOK: &str = include_str!("../../.githooks/pre-push");
const ROOT_MANIFEST: &str = include_str!("../../Cargo.toml");

/// The `msrv:` job block in `ci.yml`, from its key to the next job key.
///
/// Scoped rather than grepped whole-file: `ci.yml` pins seven toolchains and
/// runs a `cargo check` in more than one job, so an unscoped search could read
/// a neighbour's line and keep passing after the MSRV job was deleted.
fn ci_msrv_job() -> Option<&'static str> {
    let start = CI.find("\n  msrv:\n")? + 1;
    let rest = &CI[start..];
    let end = rest
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .filter(|&i| i < rest.len())
        .find(|&i| {
            let line = &rest[i..];
            line.starts_with("  ")
                && !line.starts_with("   ")
                && line[2..].starts_with(|c: char| c.is_ascii_lowercase())
        })
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The body of the `msrv:` recipe in the `justfile`: every indented line up to
/// the next thing at column 0.
fn justfile_msrv_recipe() -> Option<&'static str> {
    let start = JUSTFILE.find("\nmsrv:\n")? + "\nmsrv:\n".len();
    let rest = &JUSTFILE[start..];
    let mut end = rest.len();
    let mut cursor = 0usize;
    for line in rest.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if !bare.is_empty() && !bare.starts_with(char::is_whitespace) {
            end = cursor;
            break;
        }
        cursor += line.len();
    }
    Some(&rest[..end])
}

/// The single `cargo` line a block runs, with runs of whitespace collapsed.
///
/// Exactly one is required: a second would mean the recipe grew a step the CI
/// job does not have, which is the divergence this file exists to catch.
fn sole_cargo_command(block: &str, what: &str) -> String {
    let mut found: Vec<String> = Vec::new();
    for line in block.lines() {
        let t = line.trim();
        // `run:` is how the workflow spells it; the recipe spells it bare.
        let cmd = t.strip_prefix("run: ").unwrap_or(t);
        if cmd.starts_with("cargo ") {
            found.push(cmd.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    assert_eq!(
        found.len(),
        1,
        "{what}: expected exactly one `cargo` command, found {}: {found:?} — \
         if the MSRV gate legitimately grew a second step, both sites must \
         grow it and this helper must learn to compare the whole list",
        found.len()
    );
    found.remove(0)
}

/// Strip a `+<toolchain>` selector: the justfile picks the toolchain inline,
/// the workflow picks it with `dtolnay/rust-toolchain@`. Everything after that
/// difference must match.
fn without_toolchain_selector(cmd: &str) -> String {
    cmd.split_whitespace()
        .filter(|w| !w.starts_with('+'))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `rust-version = "X"` from `[workspace.package]`.
///
/// Anchored at column 0 so the prose mention of an unrelated crate's
/// `rust-version` further down `Cargo.toml` cannot be picked up instead.
fn declared_rust_version() -> Option<&'static str> {
    ROOT_MANIFEST
        .lines()
        .find_map(|l| l.strip_prefix("rust-version = \""))
        .and_then(|r| r.split('"').next())
}

fn ci_toolchain_version() -> Option<&'static str> {
    ci_msrv_job()?
        .lines()
        .find_map(|l| {
            // A workflow step is a YAML list item: `- uses: …`.
            let t = l.trim();
            let t = t.strip_prefix("- ").unwrap_or(t);
            t.strip_prefix("uses: dtolnay/rust-toolchain@")
        })
        .map(str::trim)
}

/// The `+1.88` in the recipe's cargo line.
fn justfile_selector_version() -> Option<String> {
    let recipe = justfile_msrv_recipe()?;
    sole_cargo_command(recipe, "justfile msrv recipe")
        .split_whitespace()
        .find_map(|w| w.strip_prefix('+').map(str::to_string))
}

/// The version inside the recipe's `rustup toolchain list | grep -q '^1\.88'`
/// installation guard.
///
/// This one earns its own assertion because it fails SILENTLY: if it drifts
/// out of step with the selector, the guard stops matching an installed
/// toolchain, the recipe prints its skip note and **exits 0**, and the hook
/// reports success having compiled nothing at the floor.
fn justfile_guard_version() -> Option<String> {
    let recipe = justfile_msrv_recipe()?;
    let line = recipe.lines().find(|l| l.contains("grep -q '^"))?;
    let after = line.split("grep -q '^").nth(1)?;
    let literal = after.split('\'').next()?;
    Some(literal.replace("\\.", "."))
}

/// **The scanners read the real files.** A guard whose source is empty passes
/// every assertion below having read nothing.
#[test]
fn the_ratchet_reads_the_real_pipeline_files() {
    for (name, src, floor) in [
        (".github/workflows/ci.yml", CI, 8_000usize),
        ("justfile", JUSTFILE, 8_000),
        (".githooks/pre-push", HOOK, 2_000),
        ("Cargo.toml", ROOT_MANIFEST, 4_000),
    ] {
        assert!(
            src.len() > floor,
            "{name}: embedded source is {} bytes, under the {floor} floor — \
             the path moved and `include_str!` is reading something else",
            src.len()
        );
    }
    assert!(
        ci_msrv_job().is_some(),
        "the `msrv:` job is gone from ci.yml, or its key is no longer at two-space indent"
    );
    assert!(
        justfile_msrv_recipe().is_some(),
        "the `msrv:` recipe is gone from the justfile"
    );
}

/// **The MSRV command is the same command in CI and locally.**
///
/// Compared to each other, never to a literal, so widening the gate in both
/// places (as #2161 PR 2 does with `--all-targets`) keeps this green while
/// widening only one place goes red.
#[test]
fn the_ci_msrv_command_equals_the_justfile_msrv_command() {
    let job = ci_msrv_job().expect("the msrv job exists");
    let recipe = justfile_msrv_recipe().expect("the msrv recipe exists");

    let ci_cmd = sole_cargo_command(job, "ci.yml msrv job");
    let just_cmd = without_toolchain_selector(&sole_cargo_command(recipe, "justfile msrv recipe"));

    assert_eq!(
        ci_cmd, just_cmd,
        "PIPELINE PARITY BROKEN: the MSRV gate runs a different command in CI \
         than locally.\n  ci.yml   : {ci_cmd}\n  justfile : {just_cmd}\n\
         Both files claim in prose to mirror each other. Change both, or change \
         the prose."
    );
}

/// **Every version in the MSRV pipeline is the declared floor.**
///
/// Four sites, not two: the manifest declares it, the workflow pins a
/// toolchain, and the recipe names it TWICE — once to select the toolchain and
/// once in the installation guard.
#[test]
fn every_msrv_version_in_the_pipeline_equals_the_declared_floor() {
    let declared = declared_rust_version()
        .expect("`rust-version = \"...\"` at column 0 in the workspace Cargo.toml");

    let ci = ci_toolchain_version()
        .expect("`uses: dtolnay/rust-toolchain@<version>` inside the msrv job");
    let selector =
        justfile_selector_version().expect("`cargo +<version>` in the justfile msrv recipe");
    let guard = justfile_guard_version()
        .expect("`grep -q '^<version>'` toolchain guard in the justfile msrv recipe");

    for (site, found) in [
        (
            ".github/workflows/ci.yml (dtolnay/rust-toolchain@)",
            ci.to_string(),
        ),
        ("justfile msrv recipe (cargo +<version>)", selector),
        ("justfile msrv recipe (rustup toolchain guard)", guard),
    ] {
        assert_eq!(
            found, declared,
            "MSRV DRIFT: {site} says {found}, but [workspace.package] \
             rust-version says {declared}. The floor is declared in Cargo.toml; \
             every gate must select that toolchain."
        );
    }
}

/// **The hook actually runs the gate it claims to mirror.**
///
/// Without this the workflow and the recipe could stay in lockstep while the
/// hook quietly stopped invoking either.
#[test]
fn the_pre_push_hook_invokes_the_msrv_recipe() {
    let invoked = HOOK
        .lines()
        .map(str::trim)
        .any(|l| l == "just msrv" || l.starts_with("just msrv "));
    assert!(
        invoked,
        ".githooks/pre-push no longer runs `just msrv` as a command. Its own \
         header still claims `just msrv` matches the MSRV job, so either the \
         gate or the claim has to change."
    );
}
