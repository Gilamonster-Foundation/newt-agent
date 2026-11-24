//! **The `confine`-off gate must be the same gate everywhere it is written down.**
//!
//! `confine` is newt-core's confinement layer (#3160): a default feature that the
//! later steps of that epic move every use of agent-bridle behind. Two lint runs of
//! newt-core alone keep it honest, each with one default feature off: the lean gate
//! (`markdown` off, `confine` kept) and the confine-off gate (`confine` off, the
//! rest kept). They are written down in the `lean-build` job of
//! `.github/workflows/ci.yml`, the `check` recipe of the `justfile`, the `windows`
//! and `macos` jobs (the confine-off gate for their platform arms) and the
//! `# CI-ONLY` header of `.githooks/pre-push`.
//!
//! The failures this guards against are quiet. Cargo features are additive, so a
//! gate that grows `--workspace`, a second `-p`, `--all-features` or a feature
//! that lists `confine` still passes, on a build that has `confine` on. And a
//! default feature added later is turned off by every `--no-default-features`
//! gate at once, so a red step no longer names its cause.
//!
//! As in `msrv_pipeline_parity_ratchet.rs`, no command is pinned as a string: the
//! commands are extracted and compared, CI to justfile, native jobs to lean job,
//! hook to lean job, and the default feature list is read from the manifest.
//! Change every site the same way and this stays green; change one and it fails.
//!
//! The lean job's probe step, which asks cargo which features it resolved, is held
//! to the confine-off gate's own feature list, on every target.
//!
//! The sources are embedded with `include_str!`, so nothing here does I/O, and
//! that is also why this file is in `exclude` in `newt-core/Cargo.toml`: it cannot
//! compile from the published package. An extractor that finds nothing panics or
//! reports a problem; none passes on an empty list.

use std::collections::BTreeMap;

const CI: &str = include_str!("../../.github/workflows/ci.yml");
const JUSTFILE: &str = include_str!("../../justfile");
const HOOK: &str = include_str!("../../.githooks/pre-push");
const MANIFEST: &str = include_str!("../Cargo.toml");

/// One job block of `ci.yml`, from its key at two-space indent to the next job key.
///
/// Scoped, not grepped whole-file: several jobs lint newt-core alone, and an unscoped
/// search would keep passing on one job's line after the other's was deleted.
fn ci_job(key: &str) -> &'static str {
    let Some(at) = CI.find(&format!("\n  {key}:\n")) else {
        panic!("ci.yml has no `{key}:` job at two-space indent");
    };
    let rest = &CI[at + 1..];
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
    &rest[..end]
}

/// The body of the `[unix] check` recipe of the `justfile`: every line up to the
/// next thing at column 0. Anchored on `[unix]` because a reduced `[windows] check`
/// recipe follows it, with no newt-core line.
fn check_recipe() -> &'static str {
    let header = "\n[unix]\ncheck:\n";
    let Some(at) = JUSTFILE.find(header) else {
        panic!("the justfile has no `[unix] check` recipe");
    };
    let rest = &JUSTFILE[at + header.len()..];
    let mut end = 0;
    for line in rest.split_inclusive('\n') {
        if !line.trim().is_empty() && !line.starts_with(char::is_whitespace) {
            break;
        }
        end += line.len();
    }
    &rest[..end]
}

/// The gates in `block`: every `cargo clippy` command that builds `-p newt-core`
/// with its default features off, whitespace collapsed, in file order.
fn gates(block: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in block.lines() {
        // `run:` is how the workflow spells a command; the recipe ends each with `|| rc=1`.
        let line = line.trim();
        let line = line.strip_prefix("run: ").unwrap_or(line);
        let line = line.strip_suffix("|| rc=1").unwrap_or(line);
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.starts_with(&["cargo", "clippy"])
            && words.windows(2).any(|pair| pair == ["-p", "newt-core"])
            && words.contains(&"--no-default-features")
        {
            found.push(words.join(" "));
        }
    }
    found
}

/// The features a gate turns on. `--features` followed by a comma list is the one
/// spelling read; any other reads as none, which fails the one-feature-off rule
/// instead of passing it.
fn enabled_features(gate: &str) -> Vec<&str> {
    let words: Vec<&str> = gate.split(' ').collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == "--features")
        .flat_map(|pair| pair[1].split(','))
        .collect()
}

/// The gates in `block` that have `confine` on (`true`) or off (`false`).
fn gates_with_confine(block: &str, on: bool) -> Vec<String> {
    gates(block)
        .into_iter()
        .filter(|gate| enabled_features(gate).contains(&"confine") == on)
        .collect()
}

/// The `[features]` table of a newt-core manifest: each feature and what it lists.
fn feature_table(manifest: &str) -> BTreeMap<String, Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        features: BTreeMap<String, Vec<String>>,
    }
    toml::from_str::<Manifest>(manifest)
        .expect("newt-core/Cargo.toml parses and has a `[features]` table")
        .features
}

/// What `default` lists in a newt-core manifest.
fn default_features(manifest: &str) -> Vec<String> {
    feature_table(manifest)
        .remove("default")
        .expect("newt-core's `[features]` table has a `default` entry")
}

/// What is wrong with the gates of the lean job and the `check` recipe, if anything.
fn gate_problems(lean_job: &str, recipe: &str, manifest: &str) -> Vec<String> {
    let default = default_features(manifest);
    let default: Vec<&str> = default.iter().map(String::as_str).collect();
    let (ci, just) = (gates(lean_job), gates(recipe));
    let mut problems = Vec::new();
    if ci != just {
        problems.push(format!(
            "PIPELINE PARITY BROKEN: ci.yml's lean-build job and the justfile `check` \
             recipe do not run the same newt-core gates.\n  ci.yml   : {ci:?}\n  \
             justfile : {just:?}\nChange both, or neither."
        ));
    }
    let mut turned_off = Vec::new();
    for gate in &ci {
        let words: Vec<&str> = gate.split(' ').collect();
        if words.iter().filter(|word| **word == "-p").count() != 1
            || words.contains(&"--workspace")
            || words.contains(&"--all-features")
        {
            problems.push(format!(
                "`{gate}` must build `-p newt-core` alone: features are additive, so a \
                 second package, `--workspace` or `--all-features` turns `confine` back on."
            ));
        }
        if !words.contains(&"--all-targets") || !gate.ends_with(" -- -D warnings") {
            problems.push(format!(
                "`{gate}` must lint every target with warnings denied: `--all-targets`, \
                 ending in `-- -D warnings`."
            ));
        }
        let on = enabled_features(gate);
        let off: Vec<&str> = default
            .iter()
            .copied()
            .filter(|feature| !on.contains(feature))
            .collect();
        if off.len() != 1 || !on.iter().all(|feature| default.contains(feature)) {
            problems.push(format!(
                "`{gate}` must turn exactly one default feature off and add no other: \
                 of {default:?} it leaves off {off:?}."
            ));
        }
        turned_off.extend(off);
    }
    let mut expected = default.clone();
    expected.sort_unstable();
    turned_off.sort_unstable();
    if turned_off != expected {
        problems.push(format!(
            "each default feature of newt-core needs one gate that turns it, and only \
             it, off: `default` is {expected:?}, the gates turn off {turned_off:?}."
        ));
    }
    problems
}

/// Each native job must run the lean job's confine-off gate, and no other gate.
fn native_leg_problems(lean_job: &str, native_job: &str) -> Vec<String> {
    let confine_off = gates_with_confine(lean_job, false);
    let native = gates(native_job);
    if !confine_off.is_empty() && native == confine_off {
        return Vec::new();
    }
    vec![format!(
        "ci.yml's native job must run the lean-build job's confine-off gate and no \
         other newt-core gate.\n  lean-build : {confine_off:?}\n  native     : {native:?}"
    )]
}

/// The lean job's probe step must resolve the confine-off gate's own feature list,
/// on every target, and its control must be that list with `confine` added.
///
/// The probe runs its own `cargo tree`, so nothing else ties it to the gate: a gate
/// that grew a feature, or a probe that resolved for the host only, would leave it
/// reporting on a configuration the gate does not lint.
fn probe_problems(lean_job: &str) -> Vec<String> {
    let Some(gate) = gates_with_confine(lean_job, false).into_iter().next() else {
        return vec!["the lean-build job has no confine-off gate for the probe to follow".into()];
    };
    let mut gate_features = enabled_features(&gate);
    gate_features.sort_unstable();
    let list = |var: &str| {
        lean_job.lines().find_map(|line| {
            let listed = line
                .trim()
                .strip_prefix(&format!("{var}=$(features "))?
                .strip_suffix(')')?;
            let mut listed: Vec<&str> = listed.split(',').collect();
            listed.sort_unstable();
            Some(listed)
        })
    };
    let mut problems = Vec::new();
    let resolves_the_gate_on_every_target = lean_job.lines().any(|line| {
        line.trim_start().starts_with("features() {")
            && line.contains("-p newt-core --no-default-features --features \"$1\"")
            && line.contains(" --target all ")
    });
    if !resolves_the_gate_on_every_target {
        problems.push(
            "the probe's `features()` must ask `cargo tree` for `-p newt-core \
             --no-default-features --features \"$1\"` on every target (`--target all`)"
                .to_string(),
        );
    }
    match (list("off"), list("on")) {
        (Some(off), Some(on)) => {
            if off != gate_features {
                problems.push(format!(
                    "the probe resolves {off:?}, the confine-off gate lints {gate_features:?}"
                ));
            }
            let mut control = gate_features.clone();
            control.push("confine");
            control.sort_unstable();
            if on != control {
                problems.push(format!(
                    "the probe's control resolves {on:?}; it must be the gate's list with \
                     `confine` added: {control:?}"
                ));
            }
        }
        _ => problems.push(
            "the lean-build job has no probe step (`on=$(features ...)` and \
             `off=$(features ...)`)"
                .to_string(),
        ),
    }
    problems
}

/// Only `default` and the two platform features may list `confine`, and all three must.
fn feature_graph_problems(manifest: &str) -> Vec<String> {
    let table = feature_table(manifest);
    let listing: Vec<&str> = table
        .iter()
        .filter(|(_, listed)| listed.iter().any(|item| item == "confine"))
        .map(|(name, _)| name.as_str())
        .collect();
    if listing == ["default", "macos-seatbelt", "windows-appcontainer"] {
        return Vec::new();
    }
    vec![format!(
        "the newt-core features that list `confine` are {listing:?}. They must be \
         `default` (a default build keeps the confinement layer), `macos-seatbelt` and \
         `windows-appcontainer` (parts of it), and no other: any other feature that \
         lists it can turn it back on in the confine-off gate."
    )]
}

/// The hook's `# CI-ONLY` header must quote the confine-off gate, on one line, as
/// the lean job runs it.
fn hook_problems(hook: &str, lean_job: &str) -> Vec<String> {
    let confine_off = gates_with_confine(lean_job, false);
    let quoted = |gate: &String| {
        hook.split_once("# CI-ONLY").is_some_and(|(_, rest)| {
            rest.lines()
                .skip(1) // the rest of the `# CI-ONLY (...)` heading line
                .take_while(|line| line.starts_with('#'))
                .any(|line| line.contains(gate.as_str()))
        })
    };
    if !confine_off.is_empty() && confine_off.iter().all(quoted) {
        return Vec::new();
    }
    vec![format!(
        ".githooks/pre-push must quote the confine-off gate in its `# CI-ONLY` header, \
         on one line and as ci.yml's lean-build job runs it: {confine_off:?}"
    )]
}

/// `text` with every `from` replaced by `to`. Panics when that changes nothing: a
/// red control whose mutation did not apply would pass having tested nothing.
fn mutated(text: &str, from: &str, to: &str) -> String {
    let out = text.replace(from, to);
    assert!(
        out != text,
        "the mutation `{from}` -> `{to}` changed nothing: the text it edits is gone. \
         If another test in this file is red too, fix that one first."
    );
    out
}

/// **The scanners read the real files.** A guard whose source is empty, or whose
/// extractor reads the wrong block, passes every assertion below having read nothing.
#[test]
fn the_ratchet_reads_the_real_files() {
    for (name, src, floor) in [
        (".github/workflows/ci.yml", CI, 8_000usize),
        ("justfile", JUSTFILE, 8_000),
        (".githooks/pre-push", HOOK, 2_000),
        ("newt-core/Cargo.toml", MANIFEST, 3_000),
    ] {
        assert!(
            src.len() > floor,
            "{name}: embedded source is {} bytes, under the {floor} floor: the path \
             moved and `include_str!` is reading something else",
            src.len()
        );
    }
    for (what, block) in [
        ("ci.yml lean-build job", ci_job("lean-build")),
        ("ci.yml windows job", ci_job("windows")),
        ("ci.yml macos job", ci_job("macos")),
        ("justfile `[unix] check` recipe", check_recipe()),
    ] {
        assert!(
            block.contains("cargo clippy "),
            "the {what} runs no clippy: the extractor is reading the wrong block"
        );
    }
    assert!(
        HOOK.contains("\n# CI-ONLY "),
        "the `# CI-ONLY` header is gone from .githooks/pre-push"
    );
    assert!(
        !default_features(MANIFEST).is_empty(),
        "newt-core's `default` feature lists nothing: the `[features]` reader lost the table"
    );
}

/// **CI and the justfile run the same gates, and each gate turns one default
/// feature off.**
///
/// Compared to each other, never to a literal. The second half is what keeps a
/// red step meaning one thing: the lean gate names `confine` and so stays the
/// markdown-off gate of #1890, and the confine-off gate keeps everything else.
#[test]
fn the_confine_off_gates_are_the_same_in_ci_and_the_justfile_and_turn_one_axis_off() {
    let problems = gate_problems(ci_job("lean-build"), check_recipe(), MANIFEST);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// **The Windows and macOS jobs run the confine-off gate the lean job runs.**
///
/// The Linux gate does not compile those platform arms, so an ungated reference
/// there needs the corresponding native lint step.
#[test]
fn the_native_jobs_run_the_same_confine_off_gate() {
    for job in ["windows", "macos"] {
        let problems = native_leg_problems(ci_job("lean-build"), ci_job(job));
        assert!(problems.is_empty(), "{job}: {}", problems.join("\n"));
    }
}

/// **A default build keeps `confine`, and only the named features turn it on.**
///
/// Once the agent-bridle dependencies sit behind `confine`, dropping it from
/// `default` would ship a newt with no confinement, silently. Membership only:
/// a later step can append `dep:` entries to `confine` without editing this.
#[test]
fn default_builds_keep_confine_and_only_the_named_features_imply_it() {
    let problems = feature_graph_problems(MANIFEST);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// **The probe asks cargo about the gate's own configuration, on every target.**
///
/// A `[target.'cfg(windows)'.dev-dependencies]` edge to a crate that takes
/// newt-core's default features turns `confine` on in the Windows job's
/// confine-off build. `cargo tree` for the host alone does not see that edge;
/// `--target all` does.
#[test]
fn the_probe_resolves_the_gates_configuration_on_every_target() {
    let problems = probe_problems(ci_job("lean-build"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// **The hook names the confine-off gate as CI-only.**
///
/// Craft Register `CRAFT-14` (v3.0): every check the pipeline runs is either
/// scoped in the pre-push hook or named there as CI-only. The hook builds default
/// features only, so it must say that CI owns this gate, and which command it is.
#[test]
fn the_pre_push_hook_names_the_confine_off_gate_as_ci_only() {
    let problems = hook_problems(HOOK, ci_job("lean-build"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// **Each check objects to what it exists to catch.**
///
/// The red controls, kept runnable: every case mutates the real text (`mutated`
/// panics if the mutation changed nothing) and the helper the tests above use
/// must then report a problem.
#[test]
fn the_checks_reject_what_they_exist_to_catch() {
    let (job, recipe) = (ci_job("lean-build"), check_recipe());
    let (on, off) = (
        gates_with_confine(job, true),
        gates_with_confine(job, false),
    );
    assert_eq!(
        (on.len(), off.len()),
        (1, 1),
        "the lean job has one gate with `confine` on and one with it off"
    );
    let (on, off) = (on[0].as_str(), off[0].as_str());
    let on_features = format!(" --features {}", enabled_features(on).join(","));

    // One site changes alone: the two no longer agree.
    let drifted = mutated(recipe, off, &off.replace(" --all-targets", ""));
    assert!(
        !gate_problems(job, &drifted, MANIFEST).is_empty(),
        "red control passed: the justfile alone dropped `--all-targets`"
    );

    // Both sites change the same way, so they still agree and only a rule can object.
    for (what, gate, from, to) in [
        ("`confine` is on", off, "--features ", "--features confine,"),
        (
            "a feature outside default",
            off,
            "--features ",
            "--features newt-core/confine,",
        ),
        ("the lean gate is unpinned", on, on_features.as_str(), ""),
        ("a second package", off, "clippy ", "clippy -p newt-tui "),
        ("`--workspace`", off, "clippy ", "clippy --workspace "),
        ("`--all-features`", off, "clippy ", "clippy --all-features "),
        ("no `--all-targets`", off, " --all-targets", ""),
        ("warnings not denied", off, " -- -D warnings", ""),
    ] {
        let broken = gate.replace(from, to);
        let problems = gate_problems(
            &mutated(job, gate, &broken),
            &mutated(recipe, gate, &broken),
            MANIFEST,
        );
        assert!(!problems.is_empty(), "red control passed: {what}");
    }

    // A default feature that no gate turns off.
    let grown = mutated(MANIFEST, "\ndefault = [", "\ndefault = [\"ast\", ");
    assert!(
        !gate_problems(job, recipe, &grown).is_empty(),
        "red control passed: `default` grew a feature and no gate changed"
    );

    for key in ["windows", "macos"] {
        for replacement in [off.replace(" --all-targets", ""), String::new()] {
            let native = mutated(ci_job(key), off, &replacement);
            assert!(
                !native_leg_problems(job, &native).is_empty(),
                "red control passed: the {key} job has a different or missing command"
            );
        }
    }

    for (from, to) in [
        ("\"markdown\", \"confine\"]", "\"markdown\"]"),
        ("\nmarkdown = [", "\nmarkdown = [\"confine\", "),
        ("appcontainer = [\"confine\", ", "appcontainer = ["),
    ] {
        assert!(
            !feature_graph_problems(&mutated(MANIFEST, from, to)).is_empty(),
            "red control passed: {from:?} became {to:?}"
        );
    }

    assert!(
        !hook_problems(&mutated(HOOK, off, ""), job).is_empty(),
        "red control passed: the hook no longer quotes the gate"
    );

    for (what, from, to) in [
        (
            "the probe resolves another list",
            "off=$(features ",
            "off=$(features ast,",
        ),
        ("the probe's control lacks `confine`", ",confine)", ")"),
        (
            "the probe resolves for the host only",
            " --target all ",
            " ",
        ),
        ("the probe step is gone", "features() {", "removed() {"),
    ] {
        assert!(
            !probe_problems(&mutated(job, from, to)).is_empty(),
            "red control passed: {what}"
        );
    }
}
