//! **The three MSRV sites must keep agreeing with each other.**
//!
//! The floor is declared once, in `[workspace.package] rust-version`, and the
//! gates READ it from there (CI and the justfile both run the same `sed`
//! reader; see `every_msrv_gate_reads_the_declared_floor_instead_of_restating_it`).
//! It is enforced in three places that all claim in prose to mirror one another:
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

/// The `sed -n '<program>' Cargo.toml` expression a gate reads the floor with.
///
/// `None` if the gate has no such reader — the gate then either restates a
/// literal or reads nothing, and the test names which gate moved.
fn floor_reader(block: &'static str) -> Option<&'static str> {
    let after = block.split("sed -n '").nth(1)?;
    let (program, rest) = after.split_once('\'')?;
    rest.trim_start()
        .starts_with("Cargo.toml")
        .then_some(program)
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

/// **Every gate READS the declared floor; none restates it.**
///
/// The floor used to be copied into four sites, and this test compared the
/// copies. The gates now read `[workspace.package] rust-version` themselves,
/// so the parity to hold is: they read it THE SAME WAY, that way finds the
/// manifest's declared line, each gate USES what it read, and no copy creeps
/// back. The justfile's installation guard earns its own assertion because it
/// fails SILENTLY: if it stops matching the installed toolchain the recipe
/// prints its skip note and exits 0, having compiled nothing at the floor.
#[test]
fn every_msrv_gate_reads_the_declared_floor_instead_of_restating_it() {
    let declared = declared_rust_version()
        .expect("`rust-version = \"...\"` at column 0 in the workspace Cargo.toml");
    let job = ci_msrv_job().expect("the msrv job exists");
    let recipe = justfile_msrv_recipe().expect("the msrv recipe exists");

    let ci_reader =
        floor_reader(job).expect("ci.yml msrv job: a `sed -n '…' Cargo.toml` step reads the floor");
    let just_reader = floor_reader(recipe)
        .expect("justfile msrv recipe: `sed -n '…' Cargo.toml` reads the floor");
    assert_eq!(
        ci_reader, just_reader,
        "PIPELINE PARITY BROKEN: CI and the justfile read the floor differently.\n  \
         ci.yml   : {ci_reader}\n  justfile : {just_reader}"
    );
    assert!(
        ci_reader.starts_with("s/^rust-version = "),
        "the reader must anchor on the manifest's declared line \
         (`rust-version = \"…\"` at column 0): {ci_reader}"
    );

    assert!(
        job.contains("toolchain: ${{ steps.msrv.outputs.version }}"),
        "ci.yml msrv job: the toolchain must be the read step's output"
    );
    assert!(
        recipe.contains("cargo +\"${msrv}\""),
        "justfile msrv recipe: the cargo selector must use the version it read"
    );
    assert!(
        recipe.contains("grep -q \"^${msrv}\""),
        "justfile msrv recipe: the installation guard must use the version it \
         read — a drifted guard skips silently and exits 0"
    );

    for (site, text) in [("ci.yml msrv job", job), ("justfile msrv recipe", recipe)] {
        assert!(
            !text.contains(declared),
            "{site} restates the floor {declared} — a second copy is the thing \
             that drifts; read it from Cargo.toml instead"
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
