//! Integration tests for `newt tunings` (show / export / import / reset).
//!
//! Every test redirects `HOME` to a private tempdir so the suite never reads
//! or writes the developer's real `~/.newt`. `Config::user_config_path`
//! resolves through `$HOME`, so the per-process env override is sufficient —
//! and because each test spawns its own `newt` process, there is no shared
//! mutable env state between parallel tests.

use assert_cmd::Command;
use predicates::prelude::*;

/// A fresh fake `$HOME` with an empty `.newt/` directory.
fn fake_home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".newt")).unwrap();
    dir
}

/// Write `~/.newt/model-capabilities.json` inside the fake home.
fn write_caps(home: &tempfile::TempDir, caps: &serde_json::Value) {
    std::fs::write(
        home.path().join(".newt").join("model-capabilities.json"),
        serde_json::to_string_pretty(caps).unwrap(),
    )
    .unwrap();
}

/// Read `~/.newt/model-capabilities.json` back as JSON.
fn read_caps(home: &tempfile::TempDir) -> serde_json::Value {
    let data =
        std::fs::read_to_string(home.path().join(".newt").join("model-capabilities.json")).unwrap();
    serde_json::from_str(&data).unwrap()
}

/// Write `~/.newt/community-tunings.toml` inside the fake home.
fn write_community(home: &tempfile::TempDir, toml_text: &str) {
    std::fs::write(
        home.path().join(".newt").join("community-tunings.toml"),
        toml_text,
    )
    .unwrap();
}

/// `newt` with `HOME` pointed at the fake home.
fn newt(home: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("HOME", home.path());
    cmd
}

/// A capabilities document with two tuned models.
fn two_model_caps() -> serde_json::Value {
    serde_json::json!({
        "alpha:7b": {
            "conformance": "full",
            "tested_date": "2026-06-01",
            "context_window": 32768,
            "safe_context": 24576,
            "tune_confidence": "high",
            "consecutive_ok": 5
        },
        "beta:13b": {
            "conformance": "partial",
            "context_window": 8192,
            "safe_context": 6144,
            "tune_confidence": "low",
            "consecutive_ok": 1
        }
    })
}

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

#[test]
fn tunings_requires_subcommand() {
    let home = fake_home();
    newt(&home).arg("tunings").assert().failure();
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

#[test]
fn show_without_data_prints_guidance() {
    let home = fake_home();
    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No tuning data found."))
        .stdout(predicate::str::contains("newt tunings import"));
}

#[test]
fn show_lists_empirical_models_with_k_formatting() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Model"))
        .stdout(predicate::str::contains("alpha:7b"))
        .stdout(predicate::str::contains("32k"))
        .stdout(predicate::str::contains("24k"))
        .stdout(predicate::str::contains("high"))
        .stdout(predicate::str::contains("beta:13b"))
        .stdout(predicate::str::contains("8k"))
        .stdout(predicate::str::contains("empirical"));
}

#[test]
fn show_filters_to_requested_model() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "show", "alpha:7b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha:7b"))
        .stdout(predicate::str::contains("beta:13b").not());
}

#[test]
fn show_unknown_model_fails_with_message() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "show", "ghost:1b"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no tuning data for model 'ghost:1b'",
        ));
}

#[test]
fn show_merges_community_profiles_as_community_source() {
    let home = fake_home();
    write_community(
        &home,
        r#"
[format]
version = "1"

[[profiles]]
model = "comm:7b"
context_window = 8192
safe_context = 6144
confidence = "medium"
"#,
    );

    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("comm:7b"))
        .stdout(predicate::str::contains("8k"))
        .stdout(predicate::str::contains("medium"))
        .stdout(predicate::str::contains("community"))
        .stdout(predicate::str::contains("Community file:"));
}

#[test]
fn show_small_context_window_is_not_abbreviated() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "tiny:1b": {
                "context_window": 512,
                "safe_context": 384,
                "tune_confidence": "low",
                "consecutive_ok": 1
            }
        }),
    );

    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("512"))
        .stdout(predicate::str::contains("384"));
}

/// Phase 20 (docs/design/model-self-tuning.md): the learned calibration
/// ratio and the thinking-only quirk render as detail lines under the row —
/// and stay absent for models that never learned them.
#[test]
fn show_renders_calibration_ratio_and_thinking_quirk() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "nemotron3:33b": {
                "context_window": 32768,
                "safe_context": 26214,
                "tune_confidence": "medium",
                "estimate_ratio": 1.29,
                "emits_thinking": true
            },
            "plain:7b": {
                "context_window": 8192,
                "safe_context": 6553
            }
        }),
    );

    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "estimate calibration: x1.29 (chars/4 -> real)",
        ))
        .stdout(predicate::str::contains(
            "quirk: emits thinking-only responses",
        ))
        // Exactly one model carries each detail line.
        .stdout(predicate::str::contains("estimate calibration").count(1))
        .stdout(predicate::str::contains("quirk:").count(1));
}

/// Phase 20: reset clears the new learned fields too — the Refused bail
/// sends users here when a learned budget (or calibration) is poisoned.
#[test]
fn reset_clears_estimate_ratio_and_emits_thinking() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "m:7b": {
                "conformance": "full",
                "safe_context": 6553,
                "estimate_ratio": 2.9,
                "emits_thinking": true
            }
        }),
    );

    newt(&home)
        .args(["tunings", "reset", "m:7b"])
        .assert()
        .success();

    let caps = read_caps(&home);
    assert!(caps["m:7b"].get("estimate_ratio").is_none());
    assert!(caps["m:7b"].get("emits_thinking").is_none());
    assert_eq!(caps["m:7b"]["conformance"], "full");
}

/// Phase 20: export carries the learned calibration ratio (additive v1 key).
#[test]
fn export_includes_estimate_ratio_when_learned() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "calibrated:33b": {
                "context_window": 32768,
                "safe_context": 26214,
                "tune_confidence": "medium",
                "consecutive_ok": 2,
                "estimate_ratio": 1.25
            }
        }),
    );

    newt(&home)
        .args(["tunings", "export"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model = \"calibrated:33b\""))
        .stdout(predicate::str::contains("estimate_ratio = 1.25"));
}

#[test]
fn show_prints_dash_for_missing_values() {
    let home = fake_home();
    write_community(
        &home,
        r#"
[format]
version = "1"

[[profiles]]
model = "bare:7b"
confidence = "none"
"#,
    );

    newt(&home)
        .args(["tunings", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bare:7b"))
        .stdout(predicate::str::contains("—"));
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[test]
fn export_prints_community_toml_to_stdout() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "export"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "# newt community model tuning profiles · format v1",
        ))
        .stdout(predicate::str::contains("[[profiles]]"))
        .stdout(predicate::str::contains("model = \"alpha:7b\""))
        .stdout(predicate::str::contains("context_window = 32768"))
        .stdout(predicate::str::contains("safe_context = 24576"))
        .stdout(predicate::str::contains("tune_source = \"empirical\""))
        .stdout(predicate::str::contains("confidence = \"high\""))
        .stdout(predicate::str::contains("data_points = 5"));
}

#[test]
fn export_writes_file_with_output_flag() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());
    let out_path = home.path().join("shared.toml");

    newt(&home)
        .args(["tunings", "export", "--output"])
        .arg(&out_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("Tunings exported to"));

    let exported = std::fs::read_to_string(&out_path).unwrap();
    assert!(exported.contains("[[profiles]]"));
    assert!(exported.contains("model = \"alpha:7b\""));
    assert!(exported.contains("model = \"beta:13b\""));
    // The export must round-trip as valid community TOML.
    let parsed: toml::Value = toml::from_str(&exported).unwrap();
    assert_eq!(
        parsed["profiles"].as_array().map(|a| a.len()),
        Some(2),
        "expected exactly two exported profiles"
    );
}

#[test]
fn export_without_data_prints_message() {
    let home = fake_home();
    newt(&home)
        .args(["tunings", "export"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No tuning data to export."));
}

#[test]
fn export_skips_models_without_tuning_fields() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "tuned:7b": {
                "context_window": 4096,
                "tune_confidence": "medium",
                "consecutive_ok": 3
            },
            "untuned:7b": {
                "conformance": "full",
                "tested_date": "2026-06-01"
            }
        }),
    );

    newt(&home)
        .args(["tunings", "export"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model = \"tuned:7b\""))
        .stdout(predicate::str::contains("untuned:7b").not());
}

#[test]
fn export_includes_community_profiles_not_in_caps() {
    let home = fake_home();
    write_caps(
        &home,
        &serde_json::json!({
            "tuned:7b": {
                "context_window": 4096,
                "tune_confidence": "medium",
                "consecutive_ok": 3
            }
        }),
    );
    write_community(
        &home,
        r#"
[format]
version = "1"

[[profiles]]
model = "community-only:7b"
safe_context = 2048
tune_source = "community"
confidence = "low"
"#,
    );

    newt(&home)
        .args(["tunings", "export"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model = \"tuned:7b\""))
        .stdout(predicate::str::contains("model = \"community-only:7b\""));
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

#[test]
fn import_creates_community_file() {
    let home = fake_home();
    let incoming = home.path().join("incoming.toml");
    std::fs::write(
        &incoming,
        r#"
[format]
version = "1"

[[profiles]]
model = "shared:7b"
context_window = 16384
safe_context = 12288
confidence = "high"
"#,
    )
    .unwrap();

    newt(&home)
        .args(["tunings", "import"])
        .arg(&incoming)
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported 1 profile(s)"))
        .stdout(predicate::str::contains("Saved to"));

    let saved =
        std::fs::read_to_string(home.path().join(".newt").join("community-tunings.toml")).unwrap();
    assert!(saved.contains("model = \"shared:7b\""));
    assert!(saved.contains("safe_context = 12288"));
}

#[test]
fn import_missing_file_fails() {
    let home = fake_home();
    newt(&home)
        .args(["tunings", "import", "/nonexistent/tunings.toml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read"));
}

#[test]
fn import_invalid_toml_fails_with_parse_error() {
    let home = fake_home();
    let bad = home.path().join("bad.toml");
    std::fs::write(&bad, "this is { not toml").unwrap();

    newt(&home)
        .args(["tunings", "import"])
        .arg(&bad)
        .assert()
        .failure()
        .stderr(predicate::str::contains("TOML parse error"));
}

#[test]
fn import_higher_confidence_replaces_existing_profile() {
    let home = fake_home();
    write_community(
        &home,
        r#"
[format]
version = "1"

[[profiles]]
model = "m:7b"
safe_context = 4096
confidence = "low"
"#,
    );
    let incoming = home.path().join("incoming.toml");
    std::fs::write(
        &incoming,
        r#"
[[profiles]]
model = "m:7b"
safe_context = 8192
confidence = "high"
"#,
    )
    .unwrap();

    newt(&home)
        .args(["tunings", "import"])
        .arg(&incoming)
        .assert()
        .success();

    let saved =
        std::fs::read_to_string(home.path().join(".newt").join("community-tunings.toml")).unwrap();
    assert!(saved.contains("safe_context = 8192"));
    assert!(!saved.contains("safe_context = 4096"));
}

#[test]
fn import_lower_confidence_keeps_existing_profile() {
    let home = fake_home();
    write_community(
        &home,
        r#"
[format]
version = "1"

[[profiles]]
model = "m:7b"
safe_context = 8192
confidence = "high"
"#,
    );
    let incoming = home.path().join("incoming.toml");
    std::fs::write(
        &incoming,
        r#"
[[profiles]]
model = "m:7b"
safe_context = 2048
confidence = "low"
"#,
    )
    .unwrap();

    newt(&home)
        .args(["tunings", "import"])
        .arg(&incoming)
        .assert()
        .success();

    let saved =
        std::fs::read_to_string(home.path().join(".newt").join("community-tunings.toml")).unwrap();
    assert!(saved.contains("safe_context = 8192"));
    assert!(!saved.contains("safe_context = 2048"));
}

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

#[test]
fn reset_single_model_clears_only_tuning_keys() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "reset", "alpha:7b"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Reset tuning data for 'alpha:7b'.",
        ));

    let caps = read_caps(&home);
    let alpha = &caps["alpha:7b"];
    // Tuning keys are gone…
    assert!(alpha.get("context_window").is_none());
    assert!(alpha.get("safe_context").is_none());
    assert!(alpha.get("tune_confidence").is_none());
    assert!(alpha.get("consecutive_ok").is_none());
    // …but the base capability fields survive.
    assert_eq!(alpha["conformance"], "full");
    assert_eq!(alpha["tested_date"], "2026-06-01");
    // And the other model is untouched.
    assert_eq!(caps["beta:13b"]["context_window"], 8192);
}

#[test]
fn reset_unknown_model_fails() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "reset", "ghost:1b"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "model 'ghost:1b' not found in capabilities cache",
        ));
}

#[test]
fn reset_all_models_clears_every_entry() {
    let home = fake_home();
    write_caps(&home, &two_model_caps());

    newt(&home)
        .args(["tunings", "reset"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Reset tuning data for 2 model(s).",
        ));

    let caps = read_caps(&home);
    for model in ["alpha:7b", "beta:13b"] {
        assert!(caps[model].get("context_window").is_none(), "{model}");
        assert!(caps[model].get("tune_confidence").is_none(), "{model}");
    }
    // Base fields survive a full reset too.
    assert_eq!(caps["alpha:7b"]["conformance"], "full");
}

#[test]
fn reset_without_caps_file_succeeds_with_zero_models() {
    let home = fake_home();

    newt(&home)
        .args(["tunings", "reset"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Reset tuning data for 0 model(s).",
        ));

    // The (empty) capabilities file is written back.
    let caps = read_caps(&home);
    assert!(caps.as_object().unwrap().is_empty());
}
